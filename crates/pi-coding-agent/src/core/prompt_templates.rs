//! Prompt templates — port of
//! `pi/packages/coding-agent/src/core/prompt-templates.ts`.
//!
//! Templates are non-recursive `*.md` files (user `agentDir/prompts`, project
//! `cwd/.pi/prompts`, plus explicit `--prompt-template` paths). Frontmatter
//! carries `description` (falling back to the first non-empty body line,
//! truncated) and `argument-hint`. Expansion replaces bash-style argument
//! placeholders; substitution is single-pass, so argument/default values are
//! never re-expanded.

use std::path::Path;

use crate::core::diagnostics::ResourceDiagnostic;
use crate::core::frontmatter::parse_frontmatter;

/// Where a template came from (the scope tag the autocomplete prefixes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemplateSource {
	User,
	Project,
	Path,
}

impl TemplateSource {
	pub fn as_str(&self) -> &'static str {
		match self {
			TemplateSource::User => "user",
			TemplateSource::Project => "project",
			TemplateSource::Path => "path",
		}
	}

	/// Upstream `getAutocompleteSourceTag` scope prefix for synthetic local
	/// sources: `u` (user), `p` (project), `t` (explicit path).
	pub fn tag(&self) -> &'static str {
		match self {
			TemplateSource::User => "u",
			TemplateSource::Project => "p",
			TemplateSource::Path => "t",
		}
	}
}

/// A prompt template loaded from a markdown file.
#[derive(Debug, Clone)]
pub struct PromptTemplate {
	pub name: String,
	pub description: String,
	pub argument_hint: Option<String>,
	pub content: String,
	pub source: TemplateSource,
	/// Absolute path to the template file.
	pub file_path: std::path::PathBuf,
}

/// Parse command arguments respecting quoted strings (bash-style); a quoted
/// empty string (`""`/`''`) is kept as an argument.
pub fn parse_command_args(args_string: &str) -> Vec<String> {
	let mut args = Vec::new();
	let mut current = String::new();
	// Tracks whether a token has started, so quoted empty strings stay.
	let mut has_token = false;
	let mut in_quote: Option<char> = None;

	for ch in args_string.chars() {
		if let Some(quote) = in_quote {
			if ch == quote {
				in_quote = None;
			} else {
				current.push(ch);
			}
		} else if ch == '"' || ch == '\'' {
			in_quote = Some(ch);
			has_token = true;
		} else if ch.is_whitespace() {
			if has_token {
				args.push(std::mem::take(&mut current));
				has_token = false;
			}
		} else {
			current.push(ch);
			has_token = true;
		}
	}

	if has_token {
		args.push(current);
	}
	args
}

/// Substitute argument placeholders in template content. Supported shapes:
/// `$1..$N` positional, `$@`/`$ARGUMENTS` for all args joined by spaces,
/// `${N:-default}` / `${@:-default}` / `${ARGUMENTS:-default}` for
/// missing-or-empty fallbacks, and `${@:N}` / `${@:N:L}` bash-style slices.
pub fn substitute_args(content: &str, args: &[String]) -> String {
	let all_args = args.join(" ");
	let pattern = regex::Regex::new(
		r"\$\{(\d+|ARGUMENTS|@):-([^}]*)\}|\$\{@:(\d+)(?::(\d+))?\}|\$(ARGUMENTS|@|\d+)",
	)
	.expect("substitution regex is static");

	pattern
		.replace_all(content, |captures: &regex::Captures| {
			let group = |index: usize| captures.get(index).map(|m| m.as_str());
			if let Some(default_target) = group(1) {
				let value = if default_target == "@" || default_target == "ARGUMENTS" {
					all_args.clone()
				} else {
					default_target
						.parse::<usize>()
						.ok()
						.and_then(|n| n.checked_sub(1))
						.and_then(|i| args.get(i))
						.cloned()
						.unwrap_or_default()
				};
				// Missing or empty falls back to the default.
				return if value.is_empty() {
					group(2).unwrap_or_default().to_string()
				} else {
					value
				};
			}

			if let Some(slice_start) = group(3) {
				let start = slice_start.parse::<usize>().unwrap_or(1).saturating_sub(1); // 1-indexed input, 0 as 1 (bash convention)
				let slice: Vec<String> = match group(4) {
					Some(length) => {
						let length = length.parse::<usize>().unwrap_or(0);
						args.iter().skip(start).take(length).cloned().collect()
					}
					None => args.iter().skip(start).cloned().collect(),
				};
				return slice.join(" ");
			}

			match group(5) {
				Some("ARGUMENTS") | Some("@") => all_args.clone(),
				Some(simple) => simple
					.parse::<usize>()
					.ok()
					.and_then(|n| n.checked_sub(1))
					.and_then(|i| args.get(i))
					.cloned()
					.unwrap_or_default(),
				None => String::new(),
			}
		})
		.into_owned()
}

/// Load one template file: name from the file stem, description from
/// frontmatter or the first non-empty body line (truncated to 60 chars).
fn load_template_from_file(
	file_path: &Path,
	source: TemplateSource,
) -> (Option<PromptTemplate>, Option<ResourceDiagnostic>) {
	let raw_content = match std::fs::read_to_string(file_path) {
		Ok(content) => content,
		Err(error) => {
			return (
				None,
				Some(ResourceDiagnostic::warning(
					error.to_string(),
					file_path.display().to_string(),
				)),
			);
		}
	};
	let frontmatter = match parse_frontmatter(&raw_content) {
		Ok(frontmatter) => frontmatter,
		Err(error) => {
			return (
				None,
				Some(ResourceDiagnostic::warning(
					error.to_string(),
					file_path.display().to_string(),
				)),
			);
		}
	};

	let name = file_path
		.file_name()
		.map(|n| n.to_string_lossy().into_owned())
		.unwrap_or_default()
		.strip_suffix(".md")
		.map(str::to_string)
		.unwrap_or_default();

	// Description from frontmatter, or the first non-empty body line,
	// truncated to 60 characters with a trailing "..." when cut.
	let mut description = frontmatter
		.get_str("description")
		.unwrap_or_default()
		.to_string();
	if description.is_empty()
		&& let Some(first_line) = frontmatter
			.body
			.lines()
			.find(|line| !line.trim().is_empty())
	{
		description = first_line.chars().take(60).collect();
		if first_line.chars().count() > 60 {
			description.push_str("...");
		}
	}

	// Upstream `...(argumentHint && { argumentHint })`: an empty hint is
	// omitted.
	let argument_hint = frontmatter
		.get_str("argument-hint")
		.filter(|hint| !hint.is_empty())
		.map(str::to_string);
	(
		Some(PromptTemplate {
			name,
			description,
			argument_hint,
			content: frontmatter.body,
			source,
			file_path: file_path.to_path_buf(),
		}),
		None,
	)
}

/// Scan a directory for `.md` files (non-recursive, symlinks resolved) and
/// load them as prompt templates.
fn load_templates_from_dir(
	dir: &Path,
	source_for: &dyn Fn(&Path) -> TemplateSource,
) -> (Vec<PromptTemplate>, Vec<ResourceDiagnostic>) {
	let mut templates = Vec::new();
	let mut diagnostics = Vec::new();

	if !dir.exists() {
		return (templates, diagnostics);
	}

	let Ok(entries) = std::fs::read_dir(dir) else {
		return (templates, diagnostics);
	};
	let mut entries: Vec<_> = entries.flatten().collect();
	entries.sort_by_key(|entry| entry.file_name());

	for entry in entries {
		let full_path = entry.path();
		// Symlinks resolve to their target; broken links are skipped.
		let is_file = match entry.file_type() {
			Ok(file_type) if file_type.is_symlink() => match std::fs::metadata(&full_path) {
				Ok(metadata) => metadata.is_file(),
				Err(_) => continue,
			},
			Ok(file_type) => file_type.is_file(),
			Err(_) => continue,
		};
		let name = entry.file_name().to_string_lossy().into_owned();
		if is_file && name.ends_with(".md") {
			let (template, problem) = load_template_from_file(&full_path, source_for(&full_path));
			if let Some(template) = template {
				templates.push(template);
			}
			diagnostics.extend(problem);
		}
	}

	(templates, diagnostics)
}

pub struct LoadPromptTemplatesOptions {
	/// Working directory for project-local templates.
	pub cwd: std::path::PathBuf,
	/// Agent config directory for global templates.
	pub agent_dir: std::path::PathBuf,
	/// Explicit prompt template paths (files or directories).
	pub prompt_paths: Vec<String>,
	/// Include default prompt directories.
	pub include_defaults: bool,
}

#[derive(Debug)]
pub struct LoadPromptTemplatesResult {
	pub templates: Vec<PromptTemplate>,
	pub diagnostics: Vec<ResourceDiagnostic>,
}

/// Load all prompt templates: user `agentDir/prompts/`, project
/// `cwd/.pi/prompts/` (defaults only), then explicit paths (missing paths are
/// skipped silently — upstream has no warning for them).
pub fn load_prompt_templates(options: LoadPromptTemplatesOptions) -> LoadPromptTemplatesResult {
	let global_prompts_dir = options.agent_dir.join("prompts");
	let project_prompts_dir = crate::config::project_pi_dir(&options.cwd).join("prompts");

	let source_for = |resolved_path: &Path| -> TemplateSource {
		if is_under_path(resolved_path, &global_prompts_dir) {
			TemplateSource::User
		} else if is_under_path(resolved_path, &project_prompts_dir) {
			TemplateSource::Project
		} else {
			TemplateSource::Path
		}
	};

	let mut templates = Vec::new();
	let mut diagnostics = Vec::new();

	if options.include_defaults {
		for dir in [&global_prompts_dir, &project_prompts_dir] {
			let (found, problems) = load_templates_from_dir(dir, &source_for);
			templates.extend(found);
			diagnostics.extend(problems);
		}
	}

	for raw_path in &options.prompt_paths {
		let resolved_path = options.cwd.join(Path::new(raw_path.trim()));
		if !resolved_path.exists() {
			continue;
		}
		let Ok(metadata) = std::fs::metadata(&resolved_path) else {
			continue;
		};
		if metadata.is_dir() {
			let (found, problems) = load_templates_from_dir(&resolved_path, &source_for);
			templates.extend(found);
			diagnostics.extend(problems);
		} else if metadata.is_file() && resolved_path.to_string_lossy().ends_with(".md") {
			let (template, problem) =
				load_template_from_file(&resolved_path, source_for(&resolved_path));
			if let Some(template) = template {
				templates.push(template);
			}
			diagnostics.extend(problem);
		}
	}

	LoadPromptTemplatesResult {
		templates,
		diagnostics,
	}
}

/// Component-wise containment (upstream `isUnderPath`).
fn is_under_path(target: &Path, root: &Path) -> bool {
	target == root || target.starts_with(root)
}

/// Expand a prompt template if the text starts with `/<name> [<args>]` and a
/// loaded template matches the name; otherwise the text passes through
/// unchanged (skill commands `/skill:…` are handled separately by the
/// session, upstream `_expandSkillCommand`).
pub fn expand_prompt_template(text: &str, templates: &[PromptTemplate]) -> String {
	let Some(after_slash) = text.strip_prefix('/') else {
		return text.to_string();
	};
	// Upstream regex `^\/([^\s]+)(?:\s+([\s\S]*))?$`: the name is the first
	// whitespace-delimited run; `\s+` then consumes the whitespace run and
	// the rest is the raw argument string.
	let Some((name, rest)) = after_slash.split_once(char::is_whitespace) else {
		if !after_slash.is_empty() {
			// `/name` with no arguments.
			if let Some(template) = templates.iter().find(|t| t.name == after_slash) {
				return substitute_args(&template.content, &[]);
			}
		}
		return text.to_string();
	};
	if name.is_empty() {
		return text.to_string();
	}
	let args_string = rest.trim_start();
	if let Some(template) = templates.iter().find(|t| t.name == name) {
		let args = parse_command_args(args_string);
		substitute_args(&template.content, &args)
	} else {
		text.to_string()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn template(name: &str, content: &str) -> PromptTemplate {
		PromptTemplate {
			name: name.into(),
			description: String::new(),
			argument_hint: None,
			content: content.into(),
			source: TemplateSource::Project,
			file_path: std::path::PathBuf::from(format!("/tmp/.pi/prompts/{name}.md")),
		}
	}

	// ---------------- parseCommandArgs ----------------

	#[test]
	fn parse_command_args_splits_and_keeps_quoted_empty_strings() {
		assert!(parse_command_args("").is_empty());
		assert!(parse_command_args("   ").is_empty());
		assert_eq!(
			parse_command_args("plain two words"),
			vec!["plain", "two", "words"]
		);
		assert_eq!(
			parse_command_args(r#""quoted empty""#),
			vec!["quoted empty"],
			"quotes strip, content keeps"
		);
		assert_eq!(
			parse_command_args("a \"\" b"),
			vec!["a", "", "b"],
			"quoted empty string stays an argument"
		);
		assert_eq!(parse_command_args("''"), vec![""]);
		assert_eq!(
			parse_command_args("echo \"hello world\" 'it''s'"),
			vec!["echo", "hello world", "its"]
		);
		assert_eq!(
			parse_command_args("mix\"quoted and\"bare"),
			vec!["mixquoted andbare"],
			"quotes do not split tokens"
		);
	}

	// ---------------- substituteArgs ----------------

	#[test]
	fn substitutes_positional_and_all_args() {
		let args: Vec<String> = ["one", "two", "three"]
			.iter()
			.map(ToString::to_string)
			.collect();
		assert_eq!(substitute_args("$1 and $2", &args), "one and two");
		assert_eq!(
			substitute_args("$4", &args),
			"",
			"missing positional is empty"
		);
		assert_eq!(substitute_args("$ARGUMENTS", &args), "one two three");
		assert_eq!(substitute_args("$@", &args), "one two three");
		assert_eq!(substitute_args("no placeholders", &args), "no placeholders");
		assert_eq!(substitute_args("$0", &args), "", "index 0 is empty");
	}

	#[test]
	fn substitutes_defaults_and_slices() {
		let args: Vec<String> = ["one", "two"].iter().map(ToString::to_string).collect();
		assert_eq!(substitute_args("${1:-fallback}", &args), "one");
		assert_eq!(substitute_args("${3:-fallback}", &args), "fallback");
		assert_eq!(
			substitute_args("${3:-}", &args),
			"",
			"empty default stays empty"
		);
		assert_eq!(substitute_args("${@:-fallback}", &args), "one two");
		assert_eq!(substitute_args("${ARGUMENTS:-fallback}", &args), "one two");
		assert_eq!(
			substitute_args("${@:-fallback}", &[]),
			"fallback",
			"empty args use the default"
		);
		assert_eq!(substitute_args("${@:2}", &args), "two");
		assert_eq!(substitute_args("${@:1}", &args), "one two");
		assert_eq!(substitute_args("${@:2:1}", &args), "two");
		assert_eq!(
			substitute_args("${@:1:1}", &args),
			"one",
			"length-limited slice"
		);
		assert_eq!(substitute_args("${@:0}", &args), "one two", "0 clamps to 1");
		assert_eq!(
			substitute_args("${@:5}", &args),
			"",
			"past-end slice is empty"
		);
	}

	#[test]
	fn substitution_is_not_recursive() {
		let args: Vec<String> = vec!["$@".into(), "$1".into()];
		assert_eq!(substitute_args("got: $1 $2", &args), "got: $@ $1");
		let with_default: Vec<String> = vec![String::new()];
		assert_eq!(substitute_args("${1:-$2}", &with_default), "$2");
	}

	// ---------------- expandPromptTemplate ----------------

	#[test]
	fn expands_named_templates_and_passes_through_misses() {
		let templates = vec![template("review", "Check $1 with $@")];
		assert_eq!(
			expand_prompt_template("/review the parser", &templates),
			"Check the with the parser"
		);
		assert_eq!(
			expand_prompt_template("/review", &templates),
			"Check  with ",
			"bare template name expands with no args"
		);
		assert_eq!(
			expand_prompt_template("/unknown args", &templates),
			"/unknown args"
		);
		assert_eq!(
			expand_prompt_template("/review extra  spaces", &templates),
			"Check extra with extra spaces",
			"$@ re-joins parsed args with single spaces (raw text spacing is not preserved)"
		);
		assert_eq!(expand_prompt_template("no slash", &templates), "no slash");
		assert_eq!(expand_prompt_template("/", &templates), "/");
		assert_eq!(
			expand_prompt_template("/skill:review", &templates),
			"/skill:review",
			"skill commands are not templates"
		);
	}

	// ---------------- loading ----------------

	fn temp_root(tag: &str) -> std::path::PathBuf {
		let dir = std::env::temp_dir().join(format!(
			"pi-templates-{tag}-{}-{}",
			std::process::id(),
			pi_ai::types::now_ms()
		));
		std::fs::create_dir_all(&dir).unwrap();
		dir
	}

	#[test]
	fn template_description_falls_back_to_truncated_first_line() {
		let root = temp_root("desc");
		let prompts = root.join("prompts");
		std::fs::create_dir_all(&prompts).unwrap();
		std::fs::write(
			prompts.join("front.md"),
			"---\ndescription: From frontmatter\nargument-hint: <file>\n---\nBody $1",
		)
		.unwrap();
		std::fs::write(
			prompts.join("short.md"),
			"First line is the description\n\nbody follows",
		)
		.unwrap();
		let long = "x".repeat(80);
		std::fs::write(prompts.join("long.md"), format!("{long}\nbody")).unwrap();
		// No description at all (empty body).
		std::fs::write(prompts.join("silent.md"), "\n   \n").unwrap();

		let result = load_prompt_templates(LoadPromptTemplatesOptions {
			cwd: root.clone(),
			agent_dir: root.clone(),
			prompt_paths: vec![],
			include_defaults: true,
		});
		assert!(result.diagnostics.is_empty(), "{result:?}");
		let by_name = |name: &str| {
			result
				.templates
				.iter()
				.find(|t| t.name == name)
				.unwrap_or_else(|| panic!("missing {name}"))
		};
		let front = by_name("front");
		assert_eq!(front.description, "From frontmatter");
		assert_eq!(front.argument_hint.as_deref(), Some("<file>"));
		assert_eq!(front.content, "Body $1");
		assert_eq!(front.source, TemplateSource::User);
		assert_eq!(
			by_name("short").description,
			"First line is the description"
		);
		let long = by_name("long");
		assert_eq!(long.description.len(), 63, "60 chars + ...");
		assert!(long.description.ends_with("..."));
		assert_eq!(by_name("silent").description, "");
		// No frontmatter means the body is the raw normalized text (not
		// trimmed — trimming only happens after a closing fence).
		assert_eq!(by_name("silent").content, "\n   \n");

		// Project dir templates tag as project; name collisions between
		// layers are possible upstream (first found wins on expansion).
		let project = root.join("project");
		std::fs::create_dir_all(project.join(".pi/prompts")).unwrap();
		std::fs::write(project.join(".pi/prompts/proj.md"), "Project one").unwrap();
		let result = load_prompt_templates(LoadPromptTemplatesOptions {
			cwd: project.clone(),
			agent_dir: root.clone(),
			prompt_paths: vec![],
			include_defaults: true,
		});
		let proj = result.templates.iter().find(|t| t.name == "proj").unwrap();
		assert_eq!(proj.source, TemplateSource::Project);
		assert_eq!(proj.description, "Project one");
		std::fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn bad_template_yaml_is_a_warning_and_skipped() {
		let root = temp_root("badyaml");
		let prompts = root.join("prompts");
		std::fs::create_dir_all(&prompts).unwrap();
		std::fs::write(prompts.join("broken.md"), "---\n: : [unclosed\n---\nbody").unwrap();
		std::fs::write(prompts.join("good.md"), "---\ndescription: fine\n---\n").unwrap();
		let result = load_prompt_templates(LoadPromptTemplatesOptions {
			cwd: root.clone(),
			agent_dir: root.clone(),
			prompt_paths: vec![],
			include_defaults: true,
		});
		assert_eq!(result.templates.len(), 1);
		assert_eq!(result.diagnostics.len(), 1);
		std::fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn non_recursive_scan_and_explicit_paths() {
		let root = temp_root("nonrec");
		let prompts = root.join("prompts");
		std::fs::create_dir_all(prompts.join("nested")).unwrap();
		std::fs::write(prompts.join("top.md"), "Top $1").unwrap();
		std::fs::write(prompts.join("nested/deep.md"), "Deep").unwrap();
		std::fs::write(prompts.join("not-txt.md.bak"), "No").unwrap();

		// Explicit directory loads non-recursively.
		let result = load_prompt_templates(LoadPromptTemplatesOptions {
			cwd: root.clone(),
			agent_dir: root.join("no-agent"),
			prompt_paths: vec![prompts.to_string_lossy().into_owned()],
			include_defaults: false,
		});
		assert_eq!(
			result
				.templates
				.iter()
				.map(|t| t.name.as_str())
				.collect::<Vec<_>>(),
			vec!["top"]
		);
		assert!(result.templates[0].source == TemplateSource::Path);

		// Explicit single file.
		let single = load_prompt_templates(LoadPromptTemplatesOptions {
			cwd: root.clone(),
			agent_dir: root.join("no-agent"),
			prompt_paths: vec![prompts.join("top.md").to_string_lossy().into_owned()],
			include_defaults: false,
		});
		assert_eq!(single.templates.len(), 1);

		// Missing explicit paths are skipped silently.
		let missing = load_prompt_templates(LoadPromptTemplatesOptions {
			cwd: root.clone(),
			agent_dir: root.join("no-agent"),
			prompt_paths: vec![root.join("missing").to_string_lossy().into_owned()],
			include_defaults: false,
		});
		assert!(missing.templates.is_empty() && missing.diagnostics.is_empty());
		std::fs::remove_dir_all(root).unwrap();
	}
}
