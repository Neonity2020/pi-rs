//! pi-rs — Rust port of the pi coding agent (`pi/packages/coding-agent`).
//!
//! Startup flow mirrors `main.ts`, reduced to the framework-stage surface:
//! parse args → load settings → resolve model → build tools → build agent
//! (system prompt) → open session → dispatch mode (interactive | print | rpc).

// Framework build: many modules intentionally ship unwired placeholder
// surfaces that later phases (providers, MCP, compaction, rpc) plug into.
#![allow(dead_code)]

mod config;
mod core;
mod modes;

use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use clap::Parser;
use pi_agent::{Agent, AgentOptions};
use pi_ai::ThinkingLevel;

use crate::core::agent_session::{restore_initial_messages, spawn_agent_session_with_options};
use crate::core::export_html;
use crate::core::model_registry::{ModelRegistry, authed_stream_fn};
use crate::core::session_manager;
use crate::core::session_manager::{
	SessionManager, SessionSelector, resolve_session_in, validate_session_dir,
};
use crate::core::settings_manager::SettingsManager;
use crate::core::tool_selection::{NoTools, ToolSelectionInput, resolve_tool_selection};
use crate::core::tools::bash::{BashTool, SessionToolContext};
use crate::core::tools::{connect_mcp_servers, create_tools};
use crate::core::ui_config;
use crate::core::{mcp_config, trust};

#[derive(Parser, Debug)]
#[command(
	name = "pi-rs",
	version,
	about = "pi coding agent — Rust port of pi.dev (framework build)",
	disable_help_subcommand = true
)]
struct Cli {
	/// Non-interactive: print the response for the prompt and exit.
	#[arg(short = 'p', long)]
	print: bool,

	/// Output mode: text (default), json, or rpc (`args.ts` `--mode`). With a
	/// TTY, `--mode text` still falls through to interactive like upstream;
	/// `--mode json` streams the event protocol without needing `-p`.
	#[arg(long, value_name = "MODE")]
	mode: Option<String>,

	/// JSONL RPC mode over stdio — legacy alias for `--mode rpc` (upstream
	/// only has `--mode`).
	#[arg(long, conflicts_with = "mode")]
	rpc: bool,

	/// Model to use: provider/id key, id, or fuzzy query.
	#[arg(short = 'm', long)]
	model: Option<String>,

	/// Restrict --model lookup to one provider.
	#[arg(long, requires = "model")]
	provider: Option<String>,

	/// API key for the selected provider (not persisted).
	#[arg(long, requires = "model")]
	api_key: Option<String>,

	/// List known models, optionally filtered by a fuzzy search pattern.
	#[arg(long, num_args = 0..=1, default_missing_value = "", value_name = "PATTERN")]
	list_models: Option<String>,

	/// Set the thinking level: off, minimal, low, medium, high, xhigh, max.
	#[arg(long, value_name = "LEVEL")]
	thinking: Option<String>,

	/// Continue the most recent session.
	#[arg(short = 'c', long, conflicts_with_all = ["resume", "no_session"])]
	r#continue: bool,

	/// Resume a session by id (sessions-dir entry, exact or unique prefix) or
	/// by path to its session directory.
	#[arg(
		long,
		visible_alias = "session",
		value_name = "ID|PATH",
		conflicts_with = "no_session"
	)]
	resume: Option<String>,

	/// Use an exact project session id (header UUIDv7), creating the session
	/// when missing (`main.ts` `--session-id`; conflicts with `--session`/
	/// `--continue`/`--resume` are validated after parsing for the upstream
	/// message).
	#[arg(long, value_name = "ID")]
	session_id: Option<String>,

	/// Fork a session by id (header UUIDv7 or directory name) or path into
	/// the current project (upstream `--fork` + `SessionManager.forkFrom`).
	#[arg(long, value_name = "ID|PATH")]
	fork: Option<String>,

	/// Directory for session storage and lookup — overrides `PI_RS_SESSION_DIR`/
	/// `PI_SESSION_DIR` and the settings `sessionDir` (upstream `--session-dir`).
	#[arg(long, value_name = "DIR")]
	session_dir: Option<String>,

	/// Export a session (id or path) to HTML and exit; an optional first
	/// positional is the output path (upstream `--export <file> [output]`).
	#[arg(long, value_name = "ID|PATH")]
	export: Option<String>,

	/// Comma-separated model patterns for Ctrl+P cycling: globs
	/// (`anthropic/*`, `*sonnet*`) or model references with an optional
	/// `:<thinking>` suffix (upstream `--models`).
	#[arg(long, value_name = "PATTERNS")]
	models: Option<String>,

	/// Set the session display name.
	#[arg(short = 'n', long, value_name = "NAME")]
	name: Option<String>,

	/// Disable session persistence.
	#[arg(long)]
	no_session: bool,

	/// Disable loading AGENTS.md and CLAUDE.md context files.
	#[arg(long)]
	no_context_files: bool,

	/// Replace the default system prompt with text or the contents of a file.
	#[arg(long, value_name = "TEXT_OR_FILE")]
	system_prompt: Option<String>,

	/// Append text or a file to the system prompt; may be repeated.
	#[arg(long, value_name = "TEXT_OR_FILE")]
	append_system_prompt: Vec<String>,

	/// Disable MCP server loading and startup.
	#[arg(long)]
	no_mcp: bool,

	/// Tool selection: an allowlist of builtin names/patterns, or only
	/// `+name`/`-name` entries editing the default set (read, bash, edit,
	/// write). Comma-separated.
	#[arg(short = 't', long, value_name = "LIST")]
	tools: Option<String>,

	/// Builtin or `mcp__<server>__<tool>` names/patterns to exclude.
	/// Comma-separated (`-xt` is the upstream short form).
	#[arg(long, value_name = "LIST")]
	exclude_tools: Option<String>,

	/// Run with no tools at all (builtins emptied, MCP bridges filtered).
	/// (`-nt` is the upstream short form.)
	#[arg(long)]
	no_tools: bool,

	/// Empty the builtin tool set but keep MCP tools available.
	/// (`-nbt` is the upstream short form.)
	#[arg(long)]
	no_builtin_tools: bool,

	/// Light theme.
	#[arg(long)]
	light: bool,

	/// Load an additional theme file (can be repeated; upstream `--theme`).
	#[arg(long, value_name = "PATH")]
	theme: Vec<String>,

	/// Use the named theme for this run — upstream `applyOverrides({ theme })`
	/// (a runtime override, not persisted).
	#[arg(long, value_name = "NAME")]
	use_theme: Option<String>,

	/// Disable theme discovery and loading; built-in themes only
	/// (upstream `--no-themes`).
	#[arg(long)]
	no_themes: bool,

	/// Trust project-local files for this run — skips the trust prompt and
	/// loads the project `.pi/mcp.json` (upstream `--approve`, `-a`).
	#[arg(short = 'a', long)]
	approve: bool,

	/// Ignore project-local files for this run — skips the trust prompt and
	/// the project `.pi/mcp.json` (upstream `--no-approve`, `-na`).
	#[arg(long)]
	no_approve: bool,

	/// Disable startup network operations, propagated as `PI_OFFLINE=1`
	/// (upstream `--offline`).
	#[arg(long)]
	offline: bool,

	/// Initial prompt (positional words are joined). Positional arguments
	/// starting with `@` are read as files and injected into the first prompt
	/// (`args.ts:247-248` — one `@` is stripped; there is no escape for a
	/// literal leading `@`).
	prompt: Vec<String>,
}

/// App mode — port of `resolveAppMode` (`main.ts:106-125`): `--mode rpc`/
/// `--mode json` win, then the print condition (`--print` or a redirected
/// stream), else interactive. Explicit `--mode text` with a TTY falls through
/// to interactive, like upstream. An invalid `--mode` value is the upstream
/// diagnostics error (`args.ts:103-109`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AppMode {
	Rpc,
	Json,
	Text,
	Interactive,
}

fn resolve_app_mode(cli: &Cli, stdin_is_tty: bool, stdout_is_tty: bool) -> anyhow::Result<AppMode> {
	if let Some(mode) = cli.mode.as_deref() {
		match mode {
			"rpc" => return Ok(AppMode::Rpc),
			"json" => return Ok(AppMode::Json),
			"text" => {}
			other => {
				anyhow::bail!("Invalid mode \"{other}\". Valid values: text, json, rpc")
			}
		}
	}
	if cli.rpc {
		return Ok(AppMode::Rpc);
	}
	if cli.print || !stdin_is_tty || !stdout_is_tty {
		return Ok(AppMode::Text);
	}
	Ok(AppMode::Interactive)
}

/// Lexical resolution of an `@file` argument relative to `cwd`
/// (`resolveToCwd` → `node:path.resolve` semantics: `~` expansion to the home
/// directory, `.`/`..` collapsing, no symlink resolution). The macOS
/// unicode-space screenshot fallbacks of `resolveReadPath` are not ported.
fn resolve_file_argument(arg: &str, cwd: &Path) -> PathBuf {
	let home = || dirs::home_dir().unwrap_or_else(|| cwd.to_path_buf());
	let mut path: PathBuf = if arg == "~" {
		home()
	} else if let Some(rest) = arg.strip_prefix("~/") {
		home().join(rest)
	} else {
		Path::new(arg).to_path_buf()
	};
	if path.is_relative() {
		path = cwd.join(path);
	}
	let mut normalized = PathBuf::new();
	for component in path.components() {
		match component {
			std::path::Component::CurDir => {}
			std::path::Component::ParentDir => {
				normalized.pop(); // clamped at the root like path.resolve
			}
			other => normalized.push(other.as_os_str()),
		}
	}
	normalized
}

/// Extensions this build treats as images for `@file` (upstream
/// `detectSupportedImageMimeTypeFromFile` sniffs magic bytes for png/jpeg/
/// webp/gif; the extension key is the pi-rs stand-in — no image pipeline).
fn is_image_extension(path: &Path) -> bool {
	matches!(
		path.extension()
			.map(|ext| ext.to_string_lossy().to_lowercase())
			.as_deref(),
		Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg" | "avif" | "heic" | "heif")
	)
}

/// Port of `cli/file-processor.ts` `processFileArguments` (text path only):
/// each argument is resolved against the cwd, BOM-stripped, and appended as a
/// `<file name="absolute path">\ncontent\n</file>\n` block. Missing files and
/// read failures print the upstream stderr line and exit 1; empty files are
/// skipped. Image attachments are not implemented in this build — image
/// extensions take the upstream `processImage`-failure text shape with a
/// not-implemented message.
fn process_file_arguments(file_args: &[String], cwd: &Path) -> String {
	let mut text = String::new();
	for file_arg in file_args {
		// `resolveReadPath`'s stripAtPrefix: a second `@` is stripped here
		// (args.ts already removed one at parse time), so `@@x` and `@x`
		// resolve alike — upstream has no escape for a literal leading `@`.
		let stripped = file_arg.strip_prefix('@').unwrap_or(file_arg);
		let absolute_path = resolve_file_argument(stripped, cwd);
		if !absolute_path.exists() {
			eprintln!("Error: File not found: {}", absolute_path.display());
			std::process::exit(1);
		}
		let Ok(metadata) = std::fs::metadata(&absolute_path) else {
			eprintln!(
				"Error: Could not read file {}: failed to stat file",
				absolute_path.display()
			);
			std::process::exit(1);
		};
		if metadata.len() == 0 {
			// Skip empty files.
			continue;
		}
		if is_image_extension(&absolute_path) {
			// Upstream failure-path shape: `text += `<file name="…">${message}</file>\n``.
			text.push_str(&format!(
				"<file name=\"{}\">Image attachments are not implemented in this build</file>\n",
				absolute_path.display()
			));
			continue;
		}
		match std::fs::read_to_string(&absolute_path) {
			Ok(content) => {
				// `stripBom` (utils/text.ts): a leading U+FEFF is dropped.
				let content = crate::core::tools::edit_diff::split_bom(&content).1;
				text.push_str(&format!(
					"<file name=\"{}\">\n{content}\n</file>\n",
					absolute_path.display()
				));
			}
			Err(error) => {
				eprintln!(
					"Error: Could not read file {}: {error}",
					absolute_path.display()
				);
				std::process::exit(1);
			}
		}
	}
	text
}

/// Port of `cli/initial-message.ts` `buildInitialMessage`: piped stdin
/// content, then `@file` text, then the first CLI message are joined with no
/// separator into the single initial prompt; the remaining messages each
/// become their own prompt.
fn build_initial_message(
	piped: Option<String>,
	file_text: &str,
	messages: &[String],
) -> (Option<String>, Vec<String>) {
	let mut parts: Vec<String> = Vec::new();
	if let Some(piped) = piped {
		parts.push(piped);
	}
	if !file_text.is_empty() {
		parts.push(file_text.to_string());
	}
	if let Some(first) = messages.first() {
		parts.push(first.clone());
	}
	let initial = (!parts.is_empty()).then(|| parts.join(""));
	(initial, messages.iter().skip(1).cloned().collect())
}

fn parse_thinking(value: Option<&str>) -> ThinkingLevel {
	match value.map(|s| s.to_lowercase()).as_deref() {
		Some("minimal") => ThinkingLevel::Minimal,
		Some("low") => ThinkingLevel::Low,
		Some("medium") => ThinkingLevel::Medium,
		Some("high") => ThinkingLevel::High,
		Some("xhigh") => ThinkingLevel::Xhigh,
		Some("max") => ThinkingLevel::Max,
		_ => ThinkingLevel::Off,
	}
}

fn is_thinking_level(level: &str) -> bool {
	matches!(
		level,
		"off" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max"
	)
}

fn split_model_thinking(query: &str) -> (&str, Option<&str>) {
	match query.rsplit_once(':') {
		Some((model, level)) if !model.is_empty() && is_thinking_level(level) => {
			(model, Some(level))
		}
		_ => (query, None),
	}
}

/// Upstream combined short flags (`args.ts` `-nt`, `-nbt`, `-xt`) that
/// clap cannot express as single tokens; rewritten to their long forms
/// before parsing.
fn rewrite_combined_short_args(args: Vec<String>) -> Vec<String> {
	let mut positional_only = false;
	args.into_iter()
		.map(|arg| {
			if arg == "--" {
				positional_only = true;
				return arg;
			}
			if positional_only {
				return arg;
			}
			match arg.as_str() {
				"-nt" => "--no-tools".into(),
				"-nbt" => "--no-builtin-tools".into(),
				"-nc" => "--no-context-files".into(),
				"-xt" => "--exclude-tools".into(),
				// `-na` is a distinct upstream flag (`--no-approve`), not a
				// `-n` cluster with value `a`.
				"-na" => "--no-approve".into(),
				other => other.to_string(),
			}
		})
		.collect()
}

/// Comma-split a tool list argument: trim, drop empties (`args.ts`).
fn split_tool_list(value: &str) -> Vec<String> {
	value
		.split(',')
		.map(|s| s.trim().to_string())
		.filter(|s| !s.is_empty())
		.collect()
}

/// `validateForkFlags` (`main.ts:299-313`): `--fork` rejects the session
/// selectors (`--session` is pi-rs's merged `--resume` flag, a documented
/// merge) with the upstream error text, exit 1.
fn validate_fork_flags(cli: &Cli) -> anyhow::Result<()> {
	if cli.fork.is_none() {
		return Ok(());
	}
	let mut conflicting = Vec::new();
	if cli.resume.is_some() {
		conflicting.push("--session");
	}
	if cli.r#continue {
		conflicting.push("--continue");
	}
	if cli.no_session {
		conflicting.push("--no-session");
	}
	if !conflicting.is_empty() {
		anyhow::bail!("--fork cannot be combined with {}", conflicting.join(", "));
	}
	Ok(())
}

/// `validateSessionIdFlags` (`main.ts:315-336`): `--session-id` rejects
/// `--session`/`--continue`/`--resume` (the merge note above applies) and the
/// id must pass `assertValidSessionId`. `--no-session` and `--fork` are
/// allowed (in-memory session with that id / fork into that id).
fn validate_session_id_flags(cli: &Cli) -> anyhow::Result<()> {
	let Some(id) = &cli.session_id else {
		return Ok(());
	};
	let mut conflicting = Vec::new();
	if cli.resume.is_some() {
		conflicting.push("--session");
	}
	if cli.r#continue {
		conflicting.push("--continue");
	}
	if !conflicting.is_empty() {
		anyhow::bail!(
			"--session-id cannot be combined with {}",
			conflicting.join(", ")
		);
	}
	session_manager::assert_valid_session_id(id).map_err(anyhow::Error::msg)
}

/// `--session-dir` resolution (`main.ts:691-695`): the CLI flag wins, then
/// `PI_RS_SESSION_DIR`/`PI_SESSION_DIR` (tilde-expanded), then the settings
/// `sessionDir`; all relative forms resolve against the startup cwd. Returns
/// the sessions ROOT — in this port every session is a subdirectory of it
/// (layout deviation, see session_manager's module docs).
fn resolve_session_root(cli: &Cli, settings_session_dir: Option<&str>, cwd: &Path) -> PathBuf {
	let from_flag = |dir: &str| resolve_file_argument(dir, cwd);
	if let Some(dir) = cli
		.session_dir
		.as_deref()
		.map(str::trim)
		.filter(|dir| !dir.is_empty())
	{
		return from_flag(dir);
	}
	for var in ["PI_RS_SESSION_DIR", "PI_SESSION_DIR"] {
		if let Ok(dir) = std::env::var(var) {
			let dir = dir.trim();
			if !dir.is_empty() {
				return from_flag(dir);
			}
		}
	}
	if let Some(dir) = settings_session_dir
		.map(str::trim)
		.filter(|dir| !dir.is_empty())
	{
		return from_flag(dir);
	}
	config::sessions_dir()
}

/// Port of `resolveSessionPath` (`main.ts:252-283`) for `--fork`/`--export`.
/// Path-like arguments (contain `/`, or end with `.jsonl`) resolve as paths
/// against the cwd; otherwise exact header-id matches win over prefix
/// matches, and the current project wins over every project in the root
/// (upstream `findById` → `list` → `listAll`). pi-rs extension: directory
/// names participate in exact/prefix matching after the header id, since
/// this port's visible session ids in `sessionFile` are directory names.
/// Returns the session directory or log path.
fn resolve_session_reference(root: &Path, cwd: &Path, arg: &str) -> anyhow::Result<PathBuf> {
	if arg.contains('/') || arg.ends_with(".jsonl") {
		return Ok(resolve_file_argument(arg, cwd));
	}
	let matches_arg = |dir: &Path, header_id: &str| -> (bool, bool) {
		let dir_name = dir.file_name().map(|n| n.to_string_lossy().into_owned());
		let dir_name = dir_name.as_deref().unwrap_or_default();
		let exact = header_id == arg || dir_name == arg;
		let prefix = header_id.starts_with(arg) || dir_name.starts_with(arg);
		(exact, prefix)
	};
	// Current project first: exact id, then newest-first prefix
	// (upstream `findById` then `list().find(startsWith)`).
	let project = session_manager::list_project_session_headers(root, cwd);
	if let Some((dir, _)) = project
		.iter()
		.find(|(dir, header)| matches_arg(dir, &header.id).0)
	{
		return Ok(dir.clone());
	}
	if let Some((dir, _)) = project
		.iter()
		.find(|(dir, header)| matches_arg(dir, &header.id).1)
	{
		return Ok(dir.clone());
	}
	// Then every project in the root (upstream `listAll`, exact before
	// prefix).
	let all = session_manager::list_session_headers(root);
	if let Some((dir, _)) = all
		.iter()
		.find(|(dir, header)| matches_arg(dir, &header.id).0)
	{
		return Ok(dir.clone());
	}
	if let Some((dir, _)) = all
		.iter()
		.find(|(dir, header)| matches_arg(dir, &header.id).1)
	{
		return Ok(dir.clone());
	}
	anyhow::bail!("No session found matching '{arg}'")
}

/// The session directory + default export basename for `--export`. Path
/// arguments may name a log file (basename minus `.jsonl`, like upstream
/// `exportFromFile`); the native layout is a session directory, whose name
/// stands in (pi-rs logs are `<dir>/session.jsonl`).
fn export_target(resolved: &Path) -> (PathBuf, String) {
	if resolved.is_dir() {
		let name = resolved
			.file_name()
			.map(|n| n.to_string_lossy().into_owned())
			.unwrap_or_else(|| "session".into());
		(resolved.to_path_buf(), name)
	} else {
		let name = resolved
			.file_stem()
			.map(|n| n.to_string_lossy().into_owned())
			.unwrap_or_else(|| "session".into());
		(resolved.parent().unwrap_or(resolved).to_path_buf(), name)
	}
}

/// `--export` (`main.ts:640-652`): resolve the session like `--fork`, render
/// it to HTML — always HTML, upstream `exportFromFile` — and exit. The first
/// positional is the output path; the default name is
/// `pi-rs-session-<basename>.html` in the cwd (upstream
/// `<APP_NAME>-session-<input basename>.html`). Success prints
/// `Exported to: <path>` and exits 0; failures print `Error: <msg>` and exit 1.
fn export_session(arg: &str, output: Option<&str>, root: &Path, cwd: &Path) -> ! {
	let result = (|| -> anyhow::Result<PathBuf> {
		let resolved = resolve_session_reference(root, cwd, arg)?;
		let (session_dir, basename) = export_target(&resolved);
		let session_file = session_dir.join("session.jsonl");
		if !session_file.exists() {
			// Upstream `exportFromFile` file-missing text.
			anyhow::bail!("File not found: {}", session_file.display());
		}
		let output = match output {
			Some(path) => resolve_file_argument(path, cwd),
			None => cwd.join(format!("{}-session-{basename}.html", config::APP_NAME)),
		};
		export_html::write_session_html(&session_dir, Some(&output))
			.map_err(|error| anyhow::anyhow!("{error}"))
	})();
	match result {
		Ok(path) => {
			println!("Exported to: {}", path.display());
			std::process::exit(0);
		}
		Err(error) => {
			eprintln!("Error: {error}");
			std::process::exit(1);
		}
	}
}

fn list_models(registry: &ModelRegistry, pattern: &str) -> String {
	// `--list-models` shows the full catalog (upstream `resolveCliModel`:
	// use *all* models — a `--models` scope only shapes cycling/picker).
	let mut models: Vec<_> = registry
		.all_models()
		.iter()
		.filter(|model| {
			pattern.is_empty()
				|| pi_tui::fuzzy::fuzzy_match(pattern, &format!("{} {}", model.provider, model.id))
					.matches
		})
		.collect();
	models.sort_by(|a, b| (&a.provider, &a.id).cmp(&(&b.provider, &b.id)));
	if models.is_empty() {
		return format!("No models matching \"{pattern}\"");
	}
	let count = |n: u64| {
		if n >= 1_000_000 {
			format!("{:.1}M", n as f64 / 1_000_000.0).replace(".0M", "M")
		} else if n >= 1_000 {
			format!("{:.1}K", n as f64 / 1_000.0).replace(".0K", "K")
		} else {
			n.to_string()
		}
	};
	let mut rows = vec![[
		"provider".to_string(),
		"model".to_string(),
		"context".to_string(),
		"max-out".to_string(),
		"thinking".to_string(),
		"images".to_string(),
	]];
	rows.extend(models.iter().map(|model| {
		[
			model.provider.clone(),
			model.id.clone(),
			count(model.context_window),
			count(model.max_tokens),
			if model.reasoning { "yes" } else { "no" }.into(),
			if model.supports_images() { "yes" } else { "no" }.into(),
		]
	}));
	let widths: Vec<_> = (0..6)
		.map(|i| {
			rows.iter()
				.map(|row| row[i].chars().count())
				.max()
				.unwrap_or(0)
		})
		.collect();
	rows.iter()
		.map(|row| {
			(0..6)
				.map(|i| format!("{:<width$}", row[i], width = widths[i]))
				.collect::<Vec<_>>()
				.join("  ")
				.trim_end()
				.to_string()
		})
		.collect::<Vec<_>>()
		.join("\n")
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
	match dotenvy::dotenv() {
		Ok(_) => {}
		Err(dotenvy::Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
		Err(error) => return Err(error.into()),
	}
	let cli = {
		let argv: Vec<String> = std::env::args().collect();
		Cli::parse_from(rewrite_combined_short_args(argv))
	};
	let startup_cwd = std::env::current_dir()?;
	if cli
		.name
		.as_deref()
		.is_some_and(|name| name.trim().is_empty())
	{
		anyhow::bail!("--name requires a non-empty value");
	}

	// Mode resolution runs before anything else so `--mode` validation errors
	// exit with the upstream diagnostics text (`resolveAppMode`).
	let stdin_is_tty = is_tty();
	let stdout_is_tty = std::io::stdout().is_terminal();
	let app_mode = resolve_app_mode(&cli, stdin_is_tty, stdout_is_tty)?;

	// `@file` extraction (`args.ts:247-248`): positionals starting with `@`
	// become file arguments (the `@` is stripped; no escape exists). The rest
	// are prompt messages; empty ones are dropped like previous pi-rs prints.
	let mut file_args: Vec<String> = Vec::new();
	let mut messages: Vec<String> = Vec::new();
	for positional in &cli.prompt {
		if let Some(file) = positional.strip_prefix('@') {
			file_args.push(file.to_string());
		} else if !positional.trim().is_empty() {
			messages.push(positional.clone());
		}
	}
	// main.ts:660-663 — rejected before any session/runtime is built.
	if app_mode == AppMode::Rpc && !file_args.is_empty() {
		anyhow::bail!("@file arguments are not supported in RPC mode");
	}

	if let Some(pattern) = &cli.list_models {
		println!("{}", list_models(&ModelRegistry::load(None), pattern));
		return Ok(());
	}

	// `--offline` (main.ts:579-583): flip the env before anything loads so
	// child processes inherit it. Upstream also sets PI_SKIP_VERSION_CHECK=1
	// here; pi-rs has no version check and no remote catalog fetch, so there
	// is nothing further to gate (declared difference).
	let offline = cli.offline
		|| std::env::var("PI_OFFLINE").is_ok_and(|value| {
			value == "1" || value.eq_ignore_ascii_case("true") || value.eq_ignore_ascii_case("yes")
		});
	if offline {
		// SAFETY: startup, before any task is spawned on the runtime — no
		// other thread reads the environment concurrently.
		unsafe { std::env::set_var("PI_OFFLINE", "1") };
	}

	// The startup settings manager provides the `sessionDir` fallback for
	// session selection (main.ts:672, 691-695); the runtime settings manager
	// below is cwd-bound and loads after the session cwd is known.
	let startup_settings = SettingsManager::load(Some(&startup_cwd));
	let session_root = resolve_session_root(
		&cli,
		startup_settings.effective().session_dir.as_deref(),
		&startup_cwd,
	);

	// `--export` (main.ts:640-652) renders and exits before any session or
	// runtime is built; the first positional is the output path.
	if let Some(arg) = &cli.export {
		export_session(
			arg,
			messages.first().map(String::as_str),
			&session_root,
			&startup_cwd,
		);
	}

	// Flag-combination validation for the new session selectors
	// (main.ts:665-666), then session selection — the port of
	// `createSessionManager` (main.ts:358-449).
	validate_fork_flags(&cli)?;
	validate_session_id_flags(&cli)?;
	let selector = if cli.r#continue {
		Some(SessionSelector::Recent)
	} else {
		cli.resume
			.as_deref()
			.map(|arg| SessionSelector::ByIdOrPath(arg.to_string()))
	};
	let opened: Option<PathBuf> = if let Some(source_arg) = &cli.fork {
		// Fork with an explicit target id checks for a collision before the
		// source is even resolved (upstream createSessionManager fork branch).
		if let Some(target_id) = &cli.session_id
			&& session_manager::find_by_header_id(&session_root, &startup_cwd, target_id).is_some()
		{
			anyhow::bail!("Session already exists with id '{target_id}'");
		}
		let source = resolve_session_reference(&session_root, &startup_cwd, source_arg)?;
		let forked = SessionManager::fork_from_in(
			&session_root,
			&source,
			&startup_cwd,
			cli.session_id.as_deref(),
		)
		.map_err(|error| anyhow::anyhow!("{error}"))?;
		Some(
			forked
				.session_dir()
				.expect("forked session is persisted")
				.to_path_buf(),
		)
	} else if cli.no_session {
		// Upstream short-circuits to an in-memory session before the
		// selector branches — no lookup, no warning (main.ts:364-366).
		None
	} else {
		match selector.as_ref() {
			Some(selector) => {
				let dir = resolve_session_in(&session_root, &startup_cwd, selector)
					.map_err(|e| anyhow::anyhow!("cannot resume session: {e}"))?;
				validate_session_dir(&dir)
					.map_err(|e| anyhow::anyhow!("cannot resume session: {e}"))?;
				Some(dir)
			}
			None => {
				// `--session-id`: open the project session with that header
				// id, or fall through to creating one (main.ts:436-448).
				cli.session_id.as_deref().and_then(|id| {
					session_manager::find_by_header_id(&session_root, &startup_cwd, id)
				})
			}
		}
	};
	if cli.fork.is_none()
		&& !cli.no_session
		&& opened.is_none()
		&& let Some(id) = &cli.session_id
	{
		// Missing project session → the upstream yellow warning, then create.
		eprintln!(
			"Warning: No project session found with id '{id}'; creating a new session with that id."
		);
	}
	let cwd = opened
		.as_ref()
		.and_then(|dir| session_manager::peek_session_header(dir))
		.map(|header| std::path::PathBuf::from(header.cwd))
		.filter(|cwd| !cwd.as_os_str().is_empty())
		.unwrap_or_else(|| startup_cwd.clone());
	if !cwd.is_dir() {
		anyhow::bail!(
			"session working directory does not exist: {}",
			cwd.display()
		);
	}
	let settings_manager = SettingsManager::load(Some(&cwd));
	let settings = settings_manager.effective();

	// Tool selection (upstream `createAgentSession` resolution): settings
	// `defaultTools` folded with `--tools`/`-xt`/`-nt`/`-nbt`. An invalid
	// `--tools` list is a diagnostics error → exit 1.
	let no_tools = if cli.no_tools {
		Some(NoTools::All)
	} else if cli.no_builtin_tools {
		Some(NoTools::Builtin)
	} else {
		None
	};
	let tools_flag = cli.tools.as_deref().map(split_tool_list);
	let exclude_flag = cli
		.exclude_tools
		.as_deref()
		.map(split_tool_list)
		.unwrap_or_default();
	let selection = resolve_tool_selection(ToolSelectionInput {
		tools: tools_flag.as_deref(),
		exclude_tools: &exclude_flag,
		no_tools,
		settings_default_tools: settings.default_tools.as_deref(),
	})
	.map_err(|error| anyhow::anyhow!("{error}"))?;

	// Model resolution.
	let default_model = match (
		settings.default_provider.as_deref(),
		settings.default_model.as_deref(),
	) {
		(Some(provider), Some(model)) if !model.starts_with(&format!("{provider}/")) => {
			Some(format!("{provider}/{model}"))
		}
		(_, Some(model)) => Some(model.to_string()),
		_ => None,
	};
	let mut registry = ModelRegistry::load(default_model.as_deref());
	// `--models` scope (`main.ts:813-817` + `resolveModelScope`): resolved
	// against the full catalog; unmatched patterns warn to stderr. Upstream
	// also falls back to the settings `enabledModels`; pi-rs has no such
	// setting yet (declared omission).
	let scope_patterns: Vec<String> = cli
		.models
		.as_deref()
		.map(|value| {
			value
				.split(',')
				.map(|pattern| pattern.trim().to_string())
				.filter(|pattern| !pattern.is_empty())
				.collect()
		})
		.unwrap_or_default();
	let (scoped, scope_warnings) = registry.resolve_scope(&scope_patterns);
	for warning in &scope_warnings {
		eprintln!("Warning: {warning}");
	}
	let (model_query, model_thinking) = cli
		.model
		.as_deref()
		.map(split_model_thinking)
		.unwrap_or(("", None));
	// Default-model selection with a scope — upstream `buildSessionOptions`
	// (main.ts:500-520): with a non-empty scope and no reopened history, the
	// saved default wins when it is in scope, otherwise the first scoped
	// model (its explicit `:<thinking>` rides along). Reopened sessions keep
	// the saved default like upstream (`hasExistingSession`).
	let mut scoped_thinking: Option<pi_ai::ThinkingLevel> = None;
	let model = match &cli.model {
		Some(_) => cli
			.provider
			.as_deref()
			.map_or_else(
				|| registry.find(model_query),
				|provider| registry.find_for_provider(provider, model_query),
			)
			.ok_or_else(|| anyhow::anyhow!("no model matching `{model_query}`"))?,
		None => {
			let saved = registry.default_model();
			let saved_in_scope = scoped.iter().position(|entry| {
				entry.model.provider == saved.provider && entry.model.id == saved.id
			});
			match (scoped.first(), saved_in_scope) {
				(Some(_), _) if opened.is_none() => {
					let index = saved_in_scope.unwrap_or(0);
					scoped_thinking = scoped[index].thinking;
					scoped[index].model.clone()
				}
				_ => saved,
			}
		}
	};
	registry.set_default(&model);
	if let Some(key) = cli.api_key.as_ref() {
		registry.set_runtime_api_key(model.provider.clone(), key.clone());
	}
	// Scope activation happens after the default marker so the scoped copies
	// the picker sees carry it. Everything reading `models()` from here on —
	// interactive picker, model cycling, rpc `get_available_models` — sees
	// the scope with no changes on their side.
	if !scoped.is_empty() {
		registry.set_model_scope(scoped.iter().map(|entry| entry.model.clone()).collect());
	}
	let models: Vec<pi_ai::Model> = registry.models().to_vec();

	// Tools + agent + system prompt. All builtin definitions are built, then
	// only the resolved selection is declared (`AgentSession._buildRuntime`
	// keeps every definition and activates a subset).
	let mut tools: Vec<Arc<dyn pi_agent::AgentTool>> = create_tools(&cwd)
		.into_iter()
		.filter(|tool| {
			selection
				.builtin_active
				.iter()
				.any(|name| name == tool.name())
		})
		.collect();
	// MCP servers — startup assembly mirroring the upstream MCP extension's
	// session_start wiring (`pi/packages/coding-agent/src/extensions/mcp/
	// index.ts`): connect every enabled server (stdio and streamable HTTP),
	// bridge its `mcp__<server>__<tool>` tools into the tool table (and with
	// it the system prompt). The config merges the global `mcp.json` with the
	// project's, gated on project trust. One server failing becomes a notice;
	// all servers failing does not block startup. The assembly must outlive
	// the agent so the server processes stay owned until exit.
	let mut mcp = if cli.no_mcp {
		crate::core::tools::McpAssembly::default()
	} else {
		// Trust gate: a project `.pi/mcp.json` loads only for trusted
		// projects. `-a`/`-na` override the decision for this run without
		// prompting or storing (upstream `projectTrustOverride`); otherwise
		// interactive runs get a one-time prompt whose answer is stored, and
		// print/rpc skip project servers with a notice instead.
		let project_trust_override = if cli.no_approve {
			Some(false)
		} else if cli.approve {
			Some(true)
		} else {
			None
		};
		let mut trust_notices: Vec<String> = Vec::new();
		resolve_project_mcp_trust(&cwd, app_mode, project_trust_override, &mut trust_notices)?;
		let mut config = mcp_config::load_mcp_config_with(
			&cwd,
			trust::resolve_project_trusted(&cwd, project_trust_override),
		)
		.await;
		for server in &mut config.servers {
			let server_cwd = server
				.cwd
				.as_deref()
				.map(std::path::Path::new)
				.unwrap_or(std::path::Path::new("."));
			server.cwd = Some(cwd.join(server_cwd).to_string_lossy().into_owned());
		}
		let mut assembly = connect_mcp_servers(&config).await;
		assembly.notices.splice(..0, trust_notices);
		// Notices are surfaced per mode at dispatch (stderr for print/rpc,
		// chat notices for the interactive TUI), not here.
		assembly
	};
	tools.extend(std::mem::take(&mut mcp.tools));
	// MCP tools face the allowlist only when it reaches MCP (upstream
	// `_allowlistFiltersMcp`); the exclude matcher always applies to them.
	let exclude_matcher = (!exclude_flag.is_empty())
		.then(|| crate::core::tool_selection::ToolMatcher::new(&exclude_flag));
	tools.retain(|tool| {
		let name = tool.name();
		if !name.starts_with("mcp__") {
			return true;
		}
		if selection
			.mcp_filter
			.as_ref()
			.is_some_and(|filter| !filter.matches(name))
		{
			return false;
		}
		!exclude_matcher
			.as_ref()
			.is_some_and(|matcher| matcher.matches(name))
	});
	let prompt_options = crate::core::system_prompt::SystemPromptOptions {
		context_files: !cli.no_context_files,
		custom: cli
			.system_prompt
			.as_deref()
			.map(crate::core::system_prompt::resolve_prompt_input),
		append: cli
			.append_system_prompt
			.iter()
			.map(|value| crate::core::system_prompt::resolve_prompt_input(value))
			.collect(),
	};
	let system_prompt =
		crate::core::system_prompt::build_system_prompt_with_options(&cwd, &tools, &prompt_options);

	// Session persistence. `--continue`/`--resume`/`--fork`/`--session-id`
	// reopen or seed from an existing log and give the agent its transcript;
	// failures exit before anything is written (no half-open session is left
	// behind).
	let mut startup_notices: Vec<String> = Vec::new();
	let (mut session, initial_messages) = if cli.no_session {
		// `--no-session --session-id` keeps the id on the in-memory header
		// (upstream `inMemory(cwd, { id })`).
		let session = match &cli.session_id {
			Some(id) => SessionManager::in_memory_with_id(id),
			None => SessionManager::in_memory(),
		};
		(session, Vec::new())
	} else {
		match opened {
			None => {
				// Fresh session; `--session-id` names the header id
				// (upstream `SessionManager.create(cwd, dir, { id })`).
				let session = SessionManager::create_in_with_options(
					&session_root,
					&cwd,
					None,
					cli.session_id.as_deref(),
				)
				.map_err(|error| anyhow::anyhow!("{error}"))?;
				(session, Vec::new())
			}
			Some(dir) => {
				let restored = restore_initial_messages(&dir);
				if cli.fork.is_some() {
					// Upstream `--fork` starts silently; the new session file
					// already carries the `parentSession` lineage.
				} else {
					startup_notices.push(format!("resuming session {}", dir.display()));
				}
				let session = SessionManager::open(&dir)?;
				(session, restored)
			}
		}
	};
	if let Some(name) = &cli.name {
		session.append_session_info(name)?;
	}

	// Session-scoped environment for the bash tool (`PI_RS_*` variables),
	// wired after session resolution so the log path is known. The tool
	// declaration is unchanged, so the already-built system prompt stays
	// valid.
	let cli_thinking = cli
		.thinking
		.as_deref()
		.filter(|value| is_thinking_level(value));
	if let Some(value) = cli
		.thinking
		.as_deref()
		.filter(|value| !is_thinking_level(value))
	{
		startup_notices.push(format!(
			"Invalid thinking level \"{value}\". Valid values: off, minimal, low, medium, high, xhigh, max"
		));
	}
	// Thinking level precedence (upstream buildSessionOptions): explicit
	// `--thinking`, then a `:<thinking>` suffix from `--model`, then the
	// chosen scoped model's explicit level, then the settings default.
	let thinking_level = if let Some(value) = cli_thinking {
		parse_thinking(Some(value))
	} else if let Some(value) = model_thinking {
		parse_thinking(Some(value))
	} else if let Some(level) = scoped_thinking {
		level
	} else {
		parse_thinking(settings.default_thinking_level.as_deref())
	};
	if let Some(index) = tools.iter().position(|tool| tool.name() == "bash") {
		let context = SessionToolContext {
			session_id: session.session_id(),
			session_file: session
				.session_dir()
				.map(|dir| dir.join("session.jsonl").to_string_lossy().into_owned()),
			provider: Some(model.provider.clone()),
			model: Some(model.id.clone()),
			reasoning_level: Some(format!("{thinking_level:?}").to_lowercase()),
		};
		tools[index] = Arc::new(BashTool::new(cwd.clone()).with_session_context(context));
	}

	let registry = Arc::new(Mutex::new(registry));
	let agent = Agent::new(AgentOptions {
		model,
		thinking_level,
		tools,
		system_prompt: Some(system_prompt),
		initial_messages,
		stream_fn: Some(authed_stream_fn(registry.clone())),
		..AgentOptions::default()
	});

	// Upstream print-mode.ts:122-127 opens the JSON stream with the session
	// header line; capture it before the manager moves into the session task
	// (in-memory sessions carry no header → no line, see print.rs).
	let session_header_line = session.header().map(|header| header.to_value().to_string());

	let handle = spawn_agent_session_with_options(
		agent,
		session,
		registry.clone(),
		settings.compaction.unwrap_or_default(),
		prompt_options,
		settings.retry.unwrap_or_default(),
	);
	handle.configure_tool_options(crate::core::agent_session::ToolRuntimeOptions {
		tools: tools_flag,
		exclude: exclude_flag,
		no_tools,
	});
	let mut mcp_notices = std::mem::take(&mut mcp.notices);
	let mcp_runtime = (!cli.no_mcp).then(|| {
		handle.configure_mcp(crate::core::tools::McpRuntime {
			assembly: mcp,
			allow: selection.mcp_filter.clone(),
			exclude: exclude_matcher.clone(),
		})
	});

	if app_mode == AppMode::Rpc {
		for notice in mcp_notices.drain(..) {
			eprintln!("mcp: {notice}");
		}
		for notice in startup_notices.drain(..) {
			eprintln!("{notice}");
		}
		let result = modes::rpc::run_rpc(handle, registry).await;
		// Session shutdown: close the MCP server transports before returning.
		if let Some(runtime) = &mcp_runtime {
			runtime.lock().await.assembly.shutdown().await;
		}
		return result;
	}

	// Print mode selection mirrors upstream main.ts resolveAppMode + the
	// stdin flip to print: JSON mode reads piped stdin too (it is prepended
	// to the initial prompt); RPC already returned above and never reads
	// stdin.
	let piped_prompt = if stdin_is_tty {
		None
	} else {
		let mut piped = String::new();
		std::io::stdin()
			.read_to_string(&mut piped)
			.ok()
			.filter(|_| !piped.trim().is_empty())
			.map(|_| piped)
	};
	// `@file` injection (cli/file-processor.ts): file blocks are appended to
	// the first prompt — stdin content, then file text, then the first
	// message, joined with no separator (buildInitialMessage).
	let file_text = process_file_arguments(&file_args, &startup_cwd);
	let (initial_message, remaining_messages) =
		build_initial_message(piped_prompt, &file_text, &messages);

	if app_mode == AppMode::Json || app_mode == AppMode::Text {
		for notice in mcp_notices.drain(..) {
			eprintln!("mcp: {notice}");
		}
		for notice in startup_notices.drain(..) {
			eprintln!("{notice}");
		}
		let mut print_prompts: Vec<String> = initial_message.into_iter().collect();
		print_prompts.extend(remaining_messages);
		let output_mode = if app_mode == AppMode::Json {
			modes::print::PrintOutputMode::Json
		} else {
			modes::print::PrintOutputMode::Text
		};
		let code = modes::print::run_print(handle, output_mode, session_header_line, print_prompts)
			.await?;
		// Close the MCP transports before the forced exit —
		// std::process::exit skips destructors, so the graceful close ladder
		// (and the orphan guard's SIGKILL) must run explicitly here.
		if let Some(runtime) = &mcp_runtime {
			runtime.lock().await.assembly.shutdown().await;
		}
		std::process::exit(code);
	}

	// Interactive: pi-rs folds every positional into one initial prompt
	// (upstream sends messages[0] and queues the rest — a pre-existing port
	// simplification). `@file` text and piped stdin prefix it the same way.
	let initial_prompt: Option<String> = {
		let mut parts: Vec<String> = Vec::new();
		if !file_text.is_empty() {
			parts.push(file_text);
		}
		let joined = messages.join(" ");
		if !joined.trim().is_empty() {
			parts.push(joined);
		}
		(!parts.is_empty()).then_some(parts.join(""))
	};

	// Theme + keybindings (ui_config.rs): user theme/keybindings from the
	// agent dir; `--light` only picks the built-in fallback theme. CLI theme
	// flags: `--use-theme` overrides the settings theme for this run (upstream
	// `applyOverrides({ theme })`, not persisted), `--theme` loads additional
	// theme files, `--no-themes` skips theme discovery (built-ins only). MCP
	// assembly and resume notices surface as chat notices inside the TUI.
	let theme_setting = cli.use_theme.clone().or_else(|| settings.theme.clone());
	let theme_files: Vec<PathBuf> = cli
		.theme
		.iter()
		.map(|path| resolve_file_argument(path, &startup_cwd))
		.collect();
	let mut ui = ui_config::load_with_cli_themes(
		theme_setting.as_deref(),
		cli.light,
		&theme_files,
		!cli.no_themes,
	);
	ui.notices
		.extend(mcp_notices.drain(..).map(|notice| format!("mcp: {notice}")));
	ui.notices.extend(startup_notices);
	let mode = modes::interactive::InteractiveMode::with_session_root(
		handle,
		models,
		registry,
		cwd.display().to_string(),
		ui,
		session_root,
	);
	let result = mode.run(initial_prompt).await;
	// Session shutdown: close the MCP server transports (graceful ladder,
	// process group reaped) before main drops the assembly.
	if let Some(runtime) = &mcp_runtime {
		runtime.lock().await.assembly.shutdown().await;
	}
	result
}

fn is_tty() -> bool {
	use std::io::IsTerminal;
	std::io::stdin().is_terminal()
}

/// Trust gate for the project-level `.pi/mcp.json` — the pi-rs cut of
/// upstream `resolveProjectTrusted` (which gates every project resource).
/// `-a`/`-na` (`projectTrustOverride`, main.ts:741-770) decide for this run
/// without prompting or touching the trust store. Trusted or absent projects
/// pass through; an interactive TTY gets a one-time prompt whose answer is
/// stored in the trust file; print/rpc/json (non-interactive) never prompt —
/// the project file is skipped with a notice. Ctrl+C at the prompt errors
/// out of startup.
fn resolve_project_mcp_trust(
	cwd: &Path,
	app_mode: AppMode,
	project_trust_override: Option<bool>,
	notices: &mut Vec<String>,
) -> anyhow::Result<()> {
	let project_config = cwd.join(".pi").join("mcp.json");
	if !project_config.exists() {
		return Ok(());
	}
	let Some(trusted_override) = project_trust_override else {
		if trust::is_trusted(cwd) {
			return Ok(());
		}
		let interactive_tty = app_mode == AppMode::Interactive;
		if !interactive_tty {
			notices.push(format!(
				"project {} exists but the project is not trusted; project MCP servers are skipped",
				project_config.display()
			));
			return Ok(());
		}
		match trust::prompt_trust(&project_config)? {
			true => trust::trust(cwd)?,
			false => notices.push(format!(
				"project {} was not trusted; project MCP servers are skipped",
				project_config.display()
			)),
		}
		return Ok(());
	};
	if !trusted_override {
		notices.push(format!(
			"project {} is ignored because the project is not trusted (--no-approve); project MCP servers are skipped",
			project_config.display()
		));
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::path::PathBuf;

	#[test]
	fn combined_aliases_respect_double_dash() {
		let args = ["pi-rs", "-nc", "-nt", "--", "-nc", "-nt"]
			.into_iter()
			.map(str::to_string)
			.collect();
		assert_eq!(
			rewrite_combined_short_args(args),
			[
				"pi-rs",
				"--no-context-files",
				"--no-tools",
				"--",
				"-nc",
				"-nt"
			]
		);
	}

	fn cli(mode: Option<&str>, print: bool, rpc: bool) -> Cli {
		Cli {
			print,
			mode: mode.map(str::to_string),
			rpc,
			model: None,
			provider: None,
			api_key: None,
			list_models: None,
			thinking: None,
			r#continue: false,
			resume: None,
			session_id: None,
			fork: None,
			session_dir: None,
			export: None,
			models: None,
			name: None,
			no_session: false,
			no_context_files: false,
			system_prompt: None,
			append_system_prompt: Vec::new(),
			no_mcp: false,
			tools: None,
			exclude_tools: None,
			no_tools: false,
			no_builtin_tools: false,
			light: false,
			theme: Vec::new(),
			use_theme: None,
			no_themes: false,
			approve: false,
			no_approve: false,
			offline: false,
			prompt: Vec::new(),
		}
	}

	// -- --mode resolution (resolveAppMode) ----------------------------------

	#[test]
	fn mode_flag_wins_over_the_print_condition() {
		// `--mode json` streams events even with a TTY and no `-p`.
		assert_eq!(
			resolve_app_mode(&cli(Some("json"), false, false), true, true).unwrap(),
			AppMode::Json
		);
		// `--mode rpc` dispatches rpc regardless of streams.
		assert_eq!(
			resolve_app_mode(&cli(Some("rpc"), false, false), true, true).unwrap(),
			AppMode::Rpc
		);
		// The legacy `--rpc` alias still dispatches rpc.
		assert_eq!(
			resolve_app_mode(&cli(None, false, true), true, true).unwrap(),
			AppMode::Rpc
		);
	}

	#[test]
	fn explicit_text_mode_falls_through_to_print_or_interactive() {
		// `--mode text` + TTY → interactive (upstream quirk preserved).
		assert_eq!(
			resolve_app_mode(&cli(Some("text"), false, false), true, true).unwrap(),
			AppMode::Interactive
		);
		// `--mode text` + `-p` → text print.
		assert_eq!(
			resolve_app_mode(&cli(Some("text"), true, false), true, true).unwrap(),
			AppMode::Text
		);
		// Redirected stdout forces print without any flag.
		assert_eq!(
			resolve_app_mode(&cli(None, false, false), true, false).unwrap(),
			AppMode::Text
		);
		assert_eq!(
			resolve_app_mode(&cli(None, false, false), false, true).unwrap(),
			AppMode::Text
		);
		// Plain TTY run stays interactive.
		assert_eq!(
			resolve_app_mode(&cli(None, false, false), true, true).unwrap(),
			AppMode::Interactive
		);
	}

	#[test]
	fn invalid_mode_value_is_the_upstream_diagnostics_error() {
		let error = resolve_app_mode(&cli(Some("xml"), false, false), true, true)
			.unwrap_err()
			.to_string();
		assert!(error.contains("Invalid mode \"xml\""), "{error}");
		assert!(error.contains("text, json, rpc"));
	}

	// -- @file argument processing --------------------------------------------

	fn temp_dir(name: &str) -> PathBuf {
		let dir =
			std::env::temp_dir().join(format!("pi-rs-fileargs-{name}-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		std::fs::create_dir_all(&dir).unwrap();
		dir
	}

	#[test]
	fn file_arguments_resolve_relative_tilde_and_dotdot() {
		let dir = temp_dir("resolve");
		let cwd = dir.join("cwd");
		std::fs::create_dir_all(&cwd).unwrap();
		assert_eq!(resolve_file_argument("a.txt", &cwd), cwd.join("a.txt"));
		assert_eq!(
			resolve_file_argument("sub/../a.txt", &cwd),
			cwd.join("a.txt")
		);
		assert_eq!(resolve_file_argument(".", &cwd), cwd);
		// `~` expands to the home directory.
		let home = dirs::home_dir().unwrap();
		assert_eq!(resolve_file_argument("~", &cwd), home);
		assert_eq!(resolve_file_argument("~/x.md", &cwd), home.join("x.md"));
		std::fs::remove_dir_all(&dir).unwrap();
	}

	#[test]
	fn file_arguments_are_injected_as_file_blocks_with_bom_stripped() {
		let dir = temp_dir("inject");
		std::fs::write(dir.join("bom.txt"), "\u{feff}bom content\n").unwrap();
		std::fs::write(dir.join("plain.txt"), "plain\ncontent").unwrap();
		std::fs::write(dir.join("empty.txt"), "").unwrap();

		let text = process_file_arguments(
			&[
				"bom.txt".to_string(),
				"empty.txt".to_string(),
				"plain.txt".to_string(),
			],
			&dir,
		);
		let abs_bom = dir.join("bom.txt").display().to_string();
		let abs_plain = dir.join("plain.txt").display().to_string();
		// The file's own trailing newline is preserved, so the wrapper adds a
		// second one before </file> (upstream template is `\n{content}\n</file>\n`).
		assert_eq!(
			text,
			format!(
				"<file name=\"{abs_bom}\">\nbom content\n\n</file>\n<file name=\"{abs_plain}\">\nplain\ncontent\n</file>\n"
			)
		);
		std::fs::remove_dir_all(&dir).unwrap();
	}

	#[test]
	fn double_at_arguments_resolve_like_single_at() {
		// Upstream strips one `@` at parse time and one more in
		// resolveReadPath, so `@@x` and `@x` resolve to the same file — no
		// escape for a literal leading `@` exists.
		let dir = temp_dir("at-at");
		std::fs::write(dir.join("x.txt"), "found").unwrap();
		let text = process_file_arguments(&["@x.txt".to_string()], &dir);
		assert!(text.contains("found"));
		std::fs::remove_dir_all(&dir).unwrap();
	}

	#[test]
	fn image_arguments_take_the_upstream_failure_text_shape() {
		let dir = temp_dir("image");
		std::fs::write(dir.join("pic.png"), b"not really a png").unwrap();
		let text = process_file_arguments(&["pic.png".to_string()], &dir);
		let abs = dir.join("pic.png").display().to_string();
		assert_eq!(
			text,
			format!(
				"<file name=\"{abs}\">Image attachments are not implemented in this build</file>\n"
			)
		);
		std::fs::remove_dir_all(&dir).unwrap();
	}

	// -- buildInitialMessage composition ---------------------------------------

	#[test]
	fn initial_message_joins_stdin_files_and_first_cli_message() {
		// stdin + file text + first message, joined with no separator; the
		// rest become their own prompts.
		let (initial, rest) = build_initial_message(
			Some("piped\n".into()),
			"<file name=\"/x\">\nfile\n</file>\n",
			&["first".into(), "second".into()],
		);
		assert_eq!(
			initial.unwrap(),
			"piped\n<file name=\"/x\">\nfile\n</file>\nfirst"
		);
		assert_eq!(rest, ["second"]);

		// No stdin/file: the first message alone is the initial prompt.
		let (initial, rest) = build_initial_message(None, "", &["only".into(), "later".into()]);
		assert_eq!(initial.unwrap(), "only");
		assert_eq!(rest, ["later"]);

		// Nothing at all → no initial prompt.
		let (initial, rest) = build_initial_message(None, "", &[]);
		assert!(initial.is_none());
		assert!(rest.is_empty());

		// Empty file text is skipped (JS truthiness), like upstream.
		let (initial, _) = build_initial_message(None, "", &["msg".into()]);
		assert_eq!(initial.unwrap(), "msg");
	}
}
