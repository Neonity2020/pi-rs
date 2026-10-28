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

use std::sync::{Arc, Mutex};

use clap::Parser;
use pi_agent::{Agent, AgentOptions};
use pi_ai::ThinkingLevel;

use crate::core::agent_session::{restore_initial_messages, spawn_agent_session_with_settings};
use crate::core::mcp_config;
use crate::core::model_registry::ModelRegistry;
use crate::core::session_manager::{
	SessionManager, SessionSelector, resolve_session, validate_session_dir,
};
use crate::core::settings_manager::SettingsManager;
use crate::core::tools::{connect_mcp_servers, create_tools};
use crate::core::ui_config;

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
	// index.ts`), reduced to the global `mcp.json`: connect every enabled
	// stdio server, bridge its `mcp__<server>__<tool>` tools into the tool
	// table (and with it the system prompt). One server failing becomes a
	// notice; all servers failing does not block startup. The assembly must
	// outlive the agent so the server processes stay owned until exit.
	let mut mcp = {
		let config = mcp_config::load_mcp_config();
		let assembly = connect_mcp_servers(&config).await;
		// Notices are surfaced per mode at dispatch (stderr for print/rpc,
		// chat notices for the interactive TUI), not here.
		assembly
	};
	tools.extend(std::mem::take(&mut mcp.tools));
	let system_prompt = crate::core::system_prompt::build_system_prompt(&cwd, &tools);

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

	let agent = Agent::new(AgentOptions {
		model,
		thinking_level: parse_thinking(settings.default_thinking_level.as_deref()),
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

	// Print mode: --print, or a prompt given while stdin is not a TTY.
	let stdin_is_tty = is_tty();
	if cli.print || (initial_prompt.is_some() && !stdin_is_tty) {
		for notice in mcp.notices.drain(..) {
			eprintln!("mcp: {notice}");
		}
		for notice in startup_notices.drain(..) {
			eprintln!("{notice}");
		}
		let prompt = initial_prompt.unwrap_or_default();
		let code = modes::print::run_print(handle, prompt).await?;
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
