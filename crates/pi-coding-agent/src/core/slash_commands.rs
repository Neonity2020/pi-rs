//! Slash commands — port of `pi/packages/coding-agent/src/core/slash-commands.ts`.
//! Built-in commands handled by the interactive mode; prompt-template and
//! skill commands join via the resource loader later.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlashCommandSource {
	Builtin,
	Prompt,
	Skill,
	Extension,
}

#[derive(Debug, Clone)]
pub struct SlashCommandInfo {
	pub name: &'static str,
	pub description: &'static str,
	pub source: SlashCommandSource,
}

pub const BUILTIN_SLASH_COMMANDS: &[SlashCommandInfo] = &[
	info(
		"help",
		"show commands and keybindings",
		SlashCommandSource::Builtin,
	),
	info(
		"exit",
		"quit the session (also /quit, /q)",
		SlashCommandSource::Builtin,
	),
	info("clear", "clear the chat view", SlashCommandSource::Builtin),
	info("new", "start a new session", SlashCommandSource::Builtin),
	info(
		"resume",
		"select or resume a saved session: /resume [id|path]",
		SlashCommandSource::Builtin,
	),
	info(
		"session",
		"show current session details",
		SlashCommandSource::Builtin,
	),
	info(
		"name",
		"set the session name: /name <name>",
		SlashCommandSource::Builtin,
	),
	info(
		"fork",
		"Create a new fork from a previous user message",
		SlashCommandSource::Builtin,
	),
	info(
		"clone",
		"Duplicate the current session at the current position",
		SlashCommandSource::Builtin,
	),
	info(
		"tree",
		"Navigate session tree (switch branches)",
		SlashCommandSource::Builtin,
	),
	info(
		"export",
		"Export session (HTML default, or specify path: .html/.jsonl)",
		SlashCommandSource::Builtin,
	),
	info(
		"import",
		"Import and resume a session from a JSONL file",
		SlashCommandSource::Builtin,
	),
	info(
		"provider",
		"add a model provider",
		SlashCommandSource::Builtin,
	),
	info(
		"login",
		"select a provider and enter an API key",
		SlashCommandSource::Builtin,
	),
	info(
		"model",
		"list models or switch: /model <query>",
		SlashCommandSource::Builtin,
	),
	info(
		"thinking",
		"cycle thinking level",
		SlashCommandSource::Builtin,
	),
	info("tools", "list active tools", SlashCommandSource::Builtin),
	info(
		"prompt",
		"show the current system prompt",
		SlashCommandSource::Builtin,
	),
	info(
		"compact",
		"summarize the conversation and replace the history",
		SlashCommandSource::Builtin,
	),
	info(
		"sessions",
		"list recent sessions (stub)",
		SlashCommandSource::Builtin,
	),
	info(
		"resume",
		"open the session selector (switch/delete/rename)",
		SlashCommandSource::Builtin,
	),
	info(
		"fork",
		"fork at a user message (selector, or /fork <n>)",
		SlashCommandSource::Builtin,
	),
	info(
		"tree",
		"show the session entry tree (read-only)",
		SlashCommandSource::Builtin,
	),
	info(
		"trust",
		"Save project trust decision for future sessions",
		SlashCommandSource::Builtin,
	),
	info(
		"import",
		"import a session from a JSONL export (/import <path>)",
		SlashCommandSource::Builtin,
	),
	info(
		"hotkeys",
		"Show all keyboard shortcuts",
		SlashCommandSource::Builtin,
	),
	info(
		"settings",
		"Open settings menu",
		SlashCommandSource::Builtin,
	),
	info(
		"scoped-models",
		"Enable/disable models for Ctrl+P cycling",
		SlashCommandSource::Builtin,
	),
	info(
		"changelog",
		"Show changelog entries",
		SlashCommandSource::Builtin,
	),
	info(
		"reload",
		"Reload keybindings, extensions, skills, prompts, themes, and context files",
		SlashCommandSource::Builtin,
	),
	info(
		"session",
		"show session info and stats",
		SlashCommandSource::Builtin,
	),
	info(
		"name",
		"show or set the session display name",
		SlashCommandSource::Builtin,
	),
	info(
		"copy",
		"Copy last agent message to clipboard",
		SlashCommandSource::Builtin,
	),
	info("new", "start a new session", SlashCommandSource::Builtin),
	info(
		"export",
		"export session (HTML default, or specify path: .html/.jsonl)",
		SlashCommandSource::Builtin,
	),
];

const fn info(
	name: &'static str,
	description: &'static str,
	source: SlashCommandSource,
) -> SlashCommandInfo {
	SlashCommandInfo {
		name,
		description,
		source,
	}
}

/// A resource-backed slash command for the menu/autocomplete listing —
/// prompt-template commands and `skill:` commands (upstream
/// `interactive-mode.ts` `createBaseAutocompleteProvider`, lines 788-818).
/// `BUILTIN_SLASH_COMMANDS` stays a static table; callers append these after
/// it.
#[derive(Debug, Clone)]
pub struct DynamicSlashCommand {
	pub name: String,
	pub description: String,
	pub source: SlashCommandSource,
	/// Template `argument-hint` frontmatter, for argument-position UIs.
	pub argument_hint: Option<String>,
}

/// Upstream `prefixAutocompleteDescription` with
/// `getAutocompleteSourceTag` reduced to the synthetic-local scope tags:
/// user → `u`, project → `p`, explicit path → `t`.
pub fn prefixed_description(description: &str, tag: &str) -> String {
	if tag.is_empty() {
		description.to_string()
	} else {
		format!("[{tag}] {description}")
	}
}

/// Build the dynamic command list from loaded resources: one
/// `/<template-name>` entry per prompt template (description = template
/// description with the source tag) and, when `enable_skill_commands` is on
/// (upstream `settingsManager.getEnableSkillCommands()`), one
/// `/skill:<name>` entry per skill (description = skill description).
/// Submission needs no registration: slash names that match no builtin fall
/// through to the prompt path where the session expands them.
pub fn dynamic_slash_commands(
	templates: &[crate::core::prompt_templates::PromptTemplate],
	skills: &[crate::core::skills::Skill],
	enable_skill_commands: bool,
) -> Vec<DynamicSlashCommand> {
	let mut commands = Vec::new();
	for template in templates {
		commands.push(DynamicSlashCommand {
			name: template.name.clone(),
			description: prefixed_description(&template.description, template.source.tag()),
			source: SlashCommandSource::Prompt,
			argument_hint: template.argument_hint.clone(),
		});
	}
	if enable_skill_commands {
		for skill in skills {
			commands.push(DynamicSlashCommand {
				name: format!("skill:{}", skill.name),
				description: prefixed_description(&skill.description, skill.source.tag()),
				source: SlashCommandSource::Skill,
				argument_hint: None,
			});
		}
	}
	commands
}

/// Extract the path argument of `/export`/`/import` — port of
/// `interactive-mode.ts` `getPathCommandArgument`: a quoted argument keeps
/// everything up to the matching quote (with no unquoting beyond the quote
/// strip); an unquoted argument ends at the first whitespace. `None` when the
/// command carries no argument.
pub fn path_command_argument<'a>(text: &'a str, command: &str) -> Option<&'a str> {
	if text == command {
		return None;
	}
	let rest = text.strip_prefix(command)?.strip_prefix(' ')?;
	let args = rest.trim_start();
	if args.is_empty() {
		return None;
	}
	let first = args.chars().next().unwrap_or_default();
	if first == '"' || first == '\'' {
		let closing = args[1..].find(first)? + 1;
		return Some(&args[1..closing]);
	}
	Some(match args.find(char::is_whitespace) {
		Some(end) => &args[..end],
		None => args,
	})
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn path_argument_handles_quotes_and_whitespace() {
		assert_eq!(path_command_argument("/export", "/export"), None);
		assert_eq!(path_command_argument("/export ", "/export"), None);
		assert_eq!(
			path_command_argument("/export out.html", "/export"),
			Some("out.html")
		);
		assert_eq!(
			path_command_argument("/export  spaced out.html tail", "/export"),
			Some("spaced")
		);
		assert_eq!(
			path_command_argument("/export \"my file.html\"", "/export"),
			Some("my file.html")
		);
		assert_eq!(
			path_command_argument("/import '/tmp/my logs/x.jsonl'", "/import"),
			Some("/tmp/my logs/x.jsonl")
		);
		// Unterminated quote: no argument (upstream returns undefined).
		assert_eq!(path_command_argument("/export \"open", "/export"), None);
		assert_eq!(path_command_argument("/model x", "/export"), None);
	}

	#[test]
	fn dynamic_commands_list_templates_and_gated_skills() {
		use crate::core::prompt_templates::{PromptTemplate, TemplateSource};
		use crate::core::skills::{Skill, SkillSource};
		let template = PromptTemplate {
			name: "review".into(),
			description: "Review code".into(),
			argument_hint: Some("<path>".into()),
			content: String::new(),
			source: TemplateSource::Project,
			file_path: std::path::PathBuf::from("/p/.pi/prompts/review.md"),
		};
		let skill = Skill {
			name: "demo".into(),
			description: "Demo skill".into(),
			file_path: std::path::PathBuf::from("/p/.pi/skills/demo/SKILL.md"),
			base_dir: std::path::PathBuf::from("/p/.pi/skills/demo"),
			source: SkillSource::User,
			disable_model_invocation: false,
		};

		let commands = dynamic_slash_commands(
			std::slice::from_ref(&template),
			std::slice::from_ref(&skill),
			true,
		);
		let rendered: Vec<(String, String, &str)> = commands
			.iter()
			.map(|command| {
				let source = match command.source {
					SlashCommandSource::Prompt => "prompt",
					SlashCommandSource::Skill => "skill",
					_ => "builtin",
				};
				(command.name.clone(), command.description.clone(), source)
			})
			.collect();
		assert_eq!(
			rendered,
			vec![
				("review".into(), "[p] Review code".into(), "prompt"),
				("skill:demo".into(), "[u] Demo skill".into(), "skill"),
			]
		);
		assert_eq!(commands[0].argument_hint.as_deref(), Some("<path>"));
		assert_eq!(commands[1].argument_hint, None);

		// Disabled skill commands drop only the skill entries.
		let commands = dynamic_slash_commands(
			std::slice::from_ref(&template),
			std::slice::from_ref(&skill),
			false,
		);
		assert_eq!(commands.len(), 1);
		assert_eq!(commands[0].name, "review");

		// Empty resources → empty dynamic list; BUILTIN stays untouched.
		assert!(dynamic_slash_commands(&[], &[], false).is_empty());
		assert!(
			BUILTIN_SLASH_COMMANDS
				.iter()
				.all(|info| info.source == SlashCommandSource::Builtin)
		);
		// The upstream tag prefixes for path-sourced resources.
		assert_eq!(prefixed_description("d", "t"), "[t] d");
		assert_eq!(prefixed_description("d", ""), "d");
	}
}
