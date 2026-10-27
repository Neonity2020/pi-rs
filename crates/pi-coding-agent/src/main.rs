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
use pi_tui::Theme;

use crate::core::agent_session::spawn_agent_session;
use crate::core::model_registry::ModelRegistry;
use crate::core::session_manager::SessionManager;
use crate::core::settings_manager::SettingsManager;
use crate::core::tools::create_tools;

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

	/// JSONL RPC mode over stdio (framing only at this stage).
	#[arg(long)]
	rpc: bool,

	/// Model to use: provider/id key, id, or fuzzy query.
	#[arg(short = 'm', long)]
	model: Option<String>,

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
		Some(query) => registry.find(query).ok_or_else(|| anyhow::anyhow!("no model matching `{query}`"))?,
		None => registry.default_model(),
	};
	registry.set_default(&model);
	let models: Vec<pi_ai::Model> = registry.models().to_vec();

	// Tools + agent + system prompt.
	let tools = create_tools(&cwd);
	let system_prompt = crate::core::system_prompt::build_system_prompt(&cwd, &tools);
	let agent = Agent::new(AgentOptions {
		model,
		thinking_level: parse_thinking(settings.default_thinking_level.as_deref()),
		tools,
		system_prompt: Some(system_prompt),
		..AgentOptions::default()
	});

	// Session persistence.
	let session = if cli.no_session {
		SessionManager::in_memory()
	} else {
		SessionManager::create(&cwd)?
	};

	let handle = spawn_agent_session(agent, session, Arc::new(Mutex::new(registry)));

	if cli.rpc {
		return modes::rpc::run_rpc().await;
	}

	let initial_prompt: Option<String> = {
		let joined = cli.prompt.join(" ");
		(!joined.trim().is_empty()).then_some(joined)
	};

	// Print mode: --print, or a prompt given while stdin is not a TTY.
	let stdin_is_tty = is_tty();
	if cli.print || (initial_prompt.is_some() && !stdin_is_tty) {
		let prompt = initial_prompt.unwrap_or_default();
		let code = modes::print::run_print(handle, prompt).await?;
		std::process::exit(code);
	}

	let theme = if cli.light { Theme::light() } else { Theme::dark() };
	let mode = modes::interactive::InteractiveMode::new(handle, models, cwd.display().to_string(), theme);
	mode.run(initial_prompt).await
}

fn is_tty() -> bool {
	use std::io::IsTerminal;
	std::io::stdin().is_terminal()
}
