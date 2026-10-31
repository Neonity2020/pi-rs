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

use crate::core::agent_session::{restore_initial_messages, spawn_agent_session_with_settings};
use crate::core::model_registry::{ModelRegistry, authed_stream_fn};
use crate::core::session_manager::{
	SessionManager, SessionSelector, resolve_session, validate_session_dir,
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

	/// Continue the most recent session.
	#[arg(long, conflicts_with_all = ["resume", "no_session"])]
	r#continue: bool,

	/// Resume a session by id (sessions-dir entry, exact or unique prefix) or
	/// by path to its session directory.
	#[arg(long, value_name = "ID|PATH", conflicts_with = "no_session")]
	resume: Option<String>,

	/// Disable session persistence.
	#[arg(long)]
	no_session: bool,

	/// Disable loading AGENTS.md and CLAUDE.md context files.
	#[arg(long)]
	no_context_files: bool,

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

/// Upstream combined short flags (`args.ts` `-nt`, `-nbt`, `-xt`) that
/// clap cannot express as single tokens; rewritten to their long forms
/// before parsing.
fn rewrite_combined_short_args(args: Vec<String>) -> Vec<String> {
	args.into_iter()
		.map(|arg| match arg.as_str() {
			"-nt" => "--no-tools".into(),
			"-nbt" => "--no-builtin-tools".into(),
			"-xt" => "--exclude-tools".into(),
			other => other.to_string(),
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
	let cwd = std::env::current_dir()?;

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

	// Settings (global + project merge).
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
	let mut registry = ModelRegistry::load(settings.default_model.as_deref());
	let model = match &cli.model {
		Some(query) => registry
			.find(query)
			.ok_or_else(|| anyhow::anyhow!("no model matching `{query}`"))?,
		None => registry.default_model(),
	};
	registry.set_default(&model);
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
	let mut mcp = {
		// Trust gate: a project `.pi/mcp.json` loads only for trusted
		// projects. Interactive runs get a one-time prompt whose answer is
		// stored; print/rpc skip project servers with a notice instead.
		let mut trust_notices: Vec<String> = Vec::new();
		resolve_project_mcp_trust(&cwd, app_mode, &mut trust_notices)?;
		let config = mcp_config::load_mcp_config().await;
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
	let system_prompt =
		crate::core::system_prompt::build_system_prompt(&cwd, &tools, !cli.no_context_files);

	// Session persistence. `--continue`/`--resume` reopen an existing log and
	// seed the agent with its transcript; failures exit before anything is
	// written (no half-open session is left behind).
	let mut startup_notices: Vec<String> = Vec::new();
	let (session, initial_messages) = if cli.no_session {
		(SessionManager::in_memory(), Vec::new())
	} else {
		let selector = if cli.r#continue {
			Some(SessionSelector::Recent)
		} else {
			cli.resume
				.as_deref()
				.map(|arg| SessionSelector::ByIdOrPath(arg.to_string()))
		};
		match selector {
			None => (SessionManager::create(&cwd)?, Vec::new()),
			Some(selector) => {
				let dir = resolve_session(&selector)
					.map_err(|e| anyhow::anyhow!("cannot resume session: {e}"))?;
				validate_session_dir(&dir)
					.map_err(|e| anyhow::anyhow!("cannot resume session: {e}"))?;
				let restored = restore_initial_messages(&dir);
				startup_notices.push(format!("resuming session {}", dir.display()));
				let session = SessionManager::open(&dir)?;
				(session, restored)
			}
		}
	};

	// Session-scoped environment for the bash tool (`PI_RS_*` variables),
	// wired after session resolution so the log path is known. The tool
	// declaration is unchanged, so the already-built system prompt stays
	// valid.
	let thinking_level = parse_thinking(settings.default_thinking_level.as_deref());
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

	let handle = spawn_agent_session_with_settings(
		agent,
		session,
		registry.clone(),
		settings.compaction.unwrap_or_default(),
	);

	if app_mode == AppMode::Rpc {
		for notice in mcp.notices.drain(..) {
			eprintln!("mcp: {notice}");
		}
		for notice in startup_notices.drain(..) {
			eprintln!("{notice}");
		}
		let result = modes::rpc::run_rpc(handle, registry).await;
		// Session shutdown: close the MCP server transports before returning.
		mcp.shutdown().await;
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
	let file_text = process_file_arguments(&file_args, &cwd);
	let (initial_message, remaining_messages) =
		build_initial_message(piped_prompt, &file_text, &messages);

	if app_mode == AppMode::Json || app_mode == AppMode::Text {
		for notice in mcp.notices.drain(..) {
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
		mcp.shutdown().await;
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
	// agent dir; `--light` only picks the built-in fallback theme. MCP
	// assembly and resume notices surface as chat notices inside the TUI.
	let mut ui = ui_config::load(settings.theme.as_deref(), cli.light);
	ui.notices
		.extend(mcp.notices.drain(..).map(|notice| format!("mcp: {notice}")));
	ui.notices.extend(startup_notices);
	let mode = modes::interactive::InteractiveMode::new(
		handle,
		models,
		registry,
		cwd.display().to_string(),
		ui,
	);
	let result = mode.run(initial_prompt).await;
	// Session shutdown: close the MCP server transports (graceful ladder,
	// process group reaped) before main drops the assembly.
	mcp.shutdown().await;
	result
}

fn is_tty() -> bool {
	use std::io::IsTerminal;
	std::io::stdin().is_terminal()
}

/// Trust gate for the project-level `.pi/mcp.json` — the pi-rs cut of
/// upstream `resolveProjectTrusted` (which gates every project resource).
/// Trusted or absent projects pass through; an interactive TTY gets a
/// one-time prompt whose answer is stored in the trust file; print/rpc/json
/// (non-interactive) never prompt — the project file is skipped with a
/// notice. Ctrl+C at the prompt errors out of startup.
fn resolve_project_mcp_trust(
	cwd: &Path,
	app_mode: AppMode,
	notices: &mut Vec<String>,
) -> anyhow::Result<()> {
	let project_config = cwd.join(".pi").join("mcp.json");
	if !project_config.exists() || trust::is_trusted(cwd) {
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
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::path::PathBuf;

	fn cli(mode: Option<&str>, print: bool, rpc: bool) -> Cli {
		Cli {
			print,
			mode: mode.map(str::to_string),
			rpc,
			model: None,
			r#continue: false,
			resume: None,
			no_session: false,
			no_context_files: false,
			tools: None,
			exclude_tools: None,
			no_tools: false,
			no_builtin_tools: false,
			light: false,
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
