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
		"fork the session at a user message (/fork lists, /fork <n>)",
		SlashCommandSource::Builtin,
	),
	info(
		"import",
		"import a session from a JSONL export (/import <path>)",
		SlashCommandSource::Builtin,
	),
	info(
		"hotkeys",
		"show the resolved keybindings",
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
		"copy the last agent message to the clipboard",
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
