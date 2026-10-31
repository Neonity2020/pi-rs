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
}
