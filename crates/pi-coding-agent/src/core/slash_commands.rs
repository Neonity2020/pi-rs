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
	info("help", "show commands and keybindings", SlashCommandSource::Builtin),
	info("exit", "quit the session (also /quit, /q)", SlashCommandSource::Builtin),
	info("clear", "clear the chat view", SlashCommandSource::Builtin),
	info("model", "list models or switch: /model <query>", SlashCommandSource::Builtin),
	info("thinking", "cycle thinking level", SlashCommandSource::Builtin),
	info("tools", "list active tools", SlashCommandSource::Builtin),
	info("prompt", "show the current system prompt", SlashCommandSource::Builtin),
	info("compact", "compact the context (stub)", SlashCommandSource::Builtin),
	info("sessions", "list recent sessions (stub)", SlashCommandSource::Builtin),
];

const fn info(
	name: &'static str,
	description: &'static str,
	source: SlashCommandSource,
) -> SlashCommandInfo {
	SlashCommandInfo { name, description, source }
}
