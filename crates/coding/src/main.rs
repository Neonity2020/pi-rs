//! pi — a minimal coding agent. Read / write / edit / bash, DeepSeek by
//! default, sessions on disk, no hidden magic.
//!
//! Layer rule: `coding` (this crate) -> `agent` -> `llm`.

mod cli;
mod modes;
mod prompt;
mod session;
mod sinks;
mod stats;
#[cfg(test)]
mod testutil;
mod tools;
mod ui;
mod util;

use std::io::IsTerminal;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use llm::{deepseek, Effort, Model, OpenAiCompat, Provider, Thinking};

#[tokio::main]
async fn main() -> Result<()> {
    // Load `.env` from the working directory or the nearest parent (dotenvy
    // walks upward). Variables already present in the real environment win.
    // No file is fine; a broken file is worth a warning.
    if let Err(err) = dotenvy::dotenv() {
        if !err.not_found() {
            eprintln!("warning: failed to load .env: {err}");
        }
    }

    let args = match cli::parse(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(message) => {
            eprintln!("error: {message}\n");
            eprint!("{}", cli::USAGE);
            std::process::exit(2);
        }
    };
    if args.help {
        print!("{}", cli::USAGE);
        return Ok(());
    }
    if args.version {
        println!("pi {} (pi-rs)", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    let cwd = std::env::current_dir().context("get working directory")?;
    let (provider, model) = build_provider(&args)?;
    let tool_list = match &args.tools {
        Some(names) => tools::specified(names).map_err(anyhow::Error::msg)?,
        None => tools::all(),
    };
    let system = prompt::build(&cwd);

    let (session, messages, resumed) = if args.continue_ {
        match session::Session::load_latest(&cwd)? {
            Some((session, messages)) => {
                let count = messages.len();
                if count > 0 {
                    eprintln!(
                        "resumed session ({count} messages) — {}",
                        session.path.display()
                    );
                }
                (session, messages, Some(count))
            }
            None => (session::Session::create(&cwd, &model.id)?, Vec::new(), None),
        }
    } else {
        (session::Session::create(&cwd, &model.id)?, Vec::new(), None)
    };
    let shared = sinks::Shared::new(session, model.clone());

    match args.mode() {
        cli::Mode::Print => {
            let prompt = resolve_prompt(&args)?;
            modes::run_print(provider, model, system, tool_list, shared, messages, prompt).await
        }
        cli::Mode::Json => {
            let prompt = resolve_prompt(&args)?;
            modes::run_json(provider, model, system, tool_list, shared, messages, prompt).await
        }
        cli::Mode::Interactive => {
            modes::run_interactive(
                provider, model, system, tool_list, shared, messages, resumed,
            )
            .await
        }
    }
}

/// A prompt from the argument, or from stdin for `pi -p` in a pipeline.
fn resolve_prompt(args: &cli::Args) -> Result<String> {
    if let Some(prompt) = &args.prompt {
        return Ok(prompt.clone());
    }
    if std::io::stdin().is_terminal() {
        bail!("no prompt given: pass one as an argument or pipe it in");
    }
    let mut buffer = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut buffer)?;
    if buffer.trim().is_empty() {
        bail!("no prompt given (empty stdin)");
    }
    Ok(buffer.trim_end().to_string())
}

fn build_provider(args: &cli::Args) -> Result<(Arc<dyn Provider>, Model)> {
    match args.provider.as_deref().unwrap_or("deepseek") {
        "deepseek" => {
            let api_key = args
                .api_key
                .clone()
                .or_else(|| std::env::var(deepseek::API_KEY_ENV).ok())
                // Treat blank values (e.g. an untouched .env template) as absent.
                .filter(|key| !key.trim().is_empty())
                .with_context(|| {
                    format!(
                        "missing API key: set {}, add it to .env, or pass --api-key",
                        deepseek::API_KEY_ENV
                    )
                })?;
            let thinking = if args.no_thinking {
                Thinking::Disabled
            } else {
                Thinking::Enabled
            };
            let effort = Some(args.effort.unwrap_or(Effort::High));
            let mut compat = OpenAiCompat::deepseek(api_key, thinking, effort);
            if let Some(base_url) = &args.base_url {
                compat = compat.with_base_url(base_url.clone());
            }
            let model_id = args
                .model
                .clone()
                .unwrap_or_else(|| deepseek::DEFAULT_MODEL.to_string());
            Ok((Arc::new(compat), deepseek::model_or_unknown(&model_id)))
        }
        "openai" => {
            let api_key = args
                .api_key
                .clone()
                .or_else(|| std::env::var("OPENAI_API_KEY").ok())
                // Treat blank values (e.g. an untouched .env template) as absent.
                .filter(|key| !key.trim().is_empty())
                .context(
                    "missing API key: set OPENAI_API_KEY, add it to .env, or pass --api-key",
                )?;
            let base_url = args
                .base_url
                .clone()
                .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
            let model_id = args
                .model
                .clone()
                .context("--model is required with --provider openai")?;
            Ok((
                Arc::new(OpenAiCompat::custom("openai", base_url, api_key)),
                Model::unknown("openai", &model_id),
            ))
        }
        other => bail!("unknown provider: {other}"),
    }
}
