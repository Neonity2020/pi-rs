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
use std::path::Path;
use std::sync::{Arc, Mutex};

use clap::Parser;
use pi_agent::{Agent, AgentOptions};
use pi_ai::ThinkingLevel;

use crate::core::agent_session::{restore_initial_messages, spawn_agent_session_with_settings};
use crate::core::model_registry::ModelRegistry;
use crate::core::session_manager::{
	SessionManager, SessionSelector, resolve_session, validate_session_dir,
};
use crate::core::settings_manager::SettingsManager;
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

	/// JSONL RPC mode over stdio.
	#[arg(long)]
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

	/// Light theme.
	#[arg(long)]
	light: bool,

	/// Initial prompt (positional words are joined).
	prompt: Vec<String>,
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
	match dotenvy::dotenv() {
		Ok(_) => {}
		Err(dotenvy::Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
		Err(error) => return Err(error.into()),
	}
	let cli = Cli::parse();
	let cwd = std::env::current_dir()?;

	// Settings (global + project merge).
	let settings_manager = SettingsManager::load(Some(&cwd));
	let settings = settings_manager.effective();

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

	// Tools + agent + system prompt.
	let mut tools = create_tools(&cwd);
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
		resolve_project_mcp_trust(&cwd, &cli, &mut trust_notices)?;
		let config = mcp_config::load_mcp_config().await;
		let mut assembly = connect_mcp_servers(&config).await;
		assembly.notices.splice(..0, trust_notices);
		// Notices are surfaced per mode at dispatch (stderr for print/rpc,
		// chat notices for the interactive TUI), not here.
		assembly
	};
	tools.extend(std::mem::take(&mut mcp.tools));
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

	let agent = Agent::new(AgentOptions {
		model,
		thinking_level,
		tools,
		system_prompt: Some(system_prompt),
		initial_messages,
		..AgentOptions::default()
	});

	let registry = Arc::new(Mutex::new(registry));
	let handle = spawn_agent_session_with_settings(
		agent,
		session,
		registry.clone(),
		settings.compaction.unwrap_or_default(),
	);

	if cli.rpc {
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

	let initial_prompt: Option<String> = {
		let joined = cli.prompt.join(" ");
		(!joined.trim().is_empty()).then_some(joined)
	};

	// Print mode selection mirrors upstream main.ts:112-123: `--print`, or
	// either stream redirected. Piped stdin is read and prepended to the
	// prompts (upstream readPipedStdin), so `echo x | pi-rs -p "summarize"`
	// works like `pi`.
	let stdin_is_tty = is_tty();
	let stdout_is_tty = std::io::stdout().is_terminal();
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
	let mut print_prompts: Vec<String> = Vec::new();
	if let Some(piped) = piped_prompt {
		print_prompts.push(piped);
	}
	print_prompts.extend(cli.prompt.iter().filter(|p| !p.trim().is_empty()).cloned());

	if cli.print || !stdin_is_tty || !stdout_is_tty {
		for notice in mcp.notices.drain(..) {
			eprintln!("mcp: {notice}");
		}
		for notice in startup_notices.drain(..) {
			eprintln!("{notice}");
		}
		let code = modes::print::run_print(handle, print_prompts).await?;
		// Close the MCP transports before the forced exit —
		// std::process::exit skips destructors, so the graceful close ladder
		// (and the orphan guard's SIGKILL) must run explicitly here.
		mcp.shutdown().await;
		std::process::exit(code);
	}

	// Theme + keybindings (ui_config.rs): user theme/keybindings from the
	// agent dir; `--light` only picks the built-in fallback theme. MCP
	// assembly and resume notices surface as chat notices inside the TUI.
	let mut ui = ui_config::load(settings.theme.as_deref(), cli.light);
	ui.notices
		.extend(mcp.notices.drain(..).map(|notice| format!("mcp: {notice}")));
	ui.notices.extend(startup_notices);
	let mode =
		modes::interactive::InteractiveMode::new(handle, models, cwd.display().to_string(), ui);
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
/// one-time prompt whose answer is stored in the trust file; print/rpc
/// (non-TTY) never prompt — the project file is skipped with a notice.
/// Ctrl+C at the prompt errors out of startup.
fn resolve_project_mcp_trust(
	cwd: &Path,
	cli: &Cli,
	notices: &mut Vec<String>,
) -> anyhow::Result<()> {
	let project_config = cwd.join(".pi").join("mcp.json");
	if !project_config.exists() || trust::is_trusted(cwd) {
		return Ok(());
	}
	let interactive_tty = !cli.rpc && !cli.print && is_tty();
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
