//! pi — a minimal coding agent. Read / write / edit / bash, DeepSeek by
//! default, sessions on disk, no hidden magic.
//!
//! Layer rule: `coding` (this crate) -> `agent` -> `llm`.

mod auth;
mod cli;
mod clipboard;
mod history;
mod login;
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

const OPENAI_BASE_URL: &str = "https://api.openai.com/v1";

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

    match args.command {
        Some(cli::Command::Login) => return login::run(&args).await,
        Some(cli::Command::Logout) => return logout(&args),
        None => {}
    }

    let cwd = std::env::current_dir().context("get working directory")?;
    let model = model_for(&args)?;
    let interactive = args.mode() == cli::Mode::Interactive;
    let (provider, logged_in) = match resolve_api_key(&args)? {
        Some(key) => (build_provider(&args, &key)?, true),
        None if interactive => (no_credentials_provider(), false),
        None => bail!(missing_key_message(&args)),
    };
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
    let summary = session.summary().map(str::to_owned);
    let shared = sinks::Shared::new(session, model.clone());
    let turn = modes::Turn {
        provider,
        model,
        system,
        tools: tool_list,
        shared,
        messages,
        summary,
    };

    match args.mode() {
        cli::Mode::Print => {
            let prompt = resolve_prompt(&args)?;
            modes::run_print(turn, prompt).await
        }
        cli::Mode::Json => {
            let prompt = resolve_prompt(&args)?;
            modes::run_json(turn, prompt).await
        }
        cli::Mode::Interactive => modes::run_interactive(turn, resumed, logged_in, args).await,
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

fn model_for(args: &cli::Args) -> Result<Model> {
    match provider_name(args) {
        "deepseek" => Ok(deepseek::model_or_unknown(
            args.model.as_deref().unwrap_or(deepseek::DEFAULT_MODEL),
        )),
        "openai" => {
            let id = args
                .model
                .as_deref()
                .context("--model is required with --provider openai")?;
            Ok(Model::unknown("openai", id))
        }
        other => bail!("unknown provider: {other}"),
    }
}

pub(crate) fn provider_name(args: &cli::Args) -> &str {
    args.provider.as_deref().unwrap_or("deepseek")
}

fn env_key_name(provider: &str) -> &'static str {
    match provider {
        "openai" => "OPENAI_API_KEY",
        _ => deepseek::API_KEY_ENV,
    }
}

/// The provider's environment key, ignoring auth.json.
pub(crate) fn env_api_key(provider: &str) -> Option<String> {
    std::env::var(env_key_name(provider))
        .ok()
        // Treat blank values (e.g. an untouched .env template) as absent.
        .filter(|key| !key.trim().is_empty())
}

pub(crate) fn default_base_url(provider: &str) -> &'static str {
    match provider {
        "openai" => OPENAI_BASE_URL,
        _ => deepseek::BASE_URL,
    }
}

/// `--base-url` > auth.json > provider default.
pub(crate) fn base_url_for(args: &cli::Args) -> Result<String> {
    if let Some(url) = &args.base_url {
        return Ok(url.clone());
    }
    if let Some(url) = auth::AuthFile::credential(provider_name(args))?.and_then(|c| c.base_url) {
        return Ok(url);
    }
    Ok(default_base_url(provider_name(args)).to_string())
}

/// `--api-key` > environment (including `.env`) > auth.json.
pub(crate) fn resolve_api_key(args: &cli::Args) -> Result<Option<String>> {
    let provider = provider_name(args);
    if let Some(key) = args.api_key.clone().filter(|key| !key.trim().is_empty()) {
        return Ok(Some(key));
    }
    if let Some(key) = env_api_key(provider) {
        return Ok(Some(key));
    }
    Ok(auth::AuthFile::credential(provider)?
        .map(|credential| credential.key)
        .filter(|key| !key.trim().is_empty()))
}

pub(crate) fn build_provider(args: &cli::Args, api_key: &str) -> Result<Arc<dyn Provider>> {
    let base_url = base_url_for(args)?;
    match provider_name(args) {
        "deepseek" => {
            let thinking = if args.no_thinking {
                Thinking::Disabled
            } else {
                Thinking::Enabled
            };
            let effort = Some(args.effort.unwrap_or(Effort::High));
            Ok(Arc::new(
                OpenAiCompat::deepseek(api_key, thinking, effort).with_base_url(base_url),
            ))
        }
        "openai" => Ok(Arc::new(OpenAiCompat::custom("openai", base_url, api_key))),
        other => bail!("unknown provider: {other}"),
    }
}

fn missing_key_message(args: &cli::Args) -> String {
    let provider = provider_name(args);
    let login = if provider == "openai" {
        "pi login --provider openai"
    } else {
        "pi login"
    };
    format!(
        "missing API key: run `{login}`, set {}, add it to .env, or pass --api-key",
        env_key_name(provider)
    )
}

fn logout(args: &cli::Args) -> Result<()> {
    let provider = provider_name(args);
    if auth::AuthFile::remove(provider)? {
        println!("removed saved credentials for {provider}");
    } else {
        println!("no saved credentials for {provider}");
    }
    if env_api_key(provider).is_some() {
        println!(
            "note: {} is still set and will keep being used",
            env_key_name(provider)
        );
    } else if args
        .api_key
        .as_deref()
        .is_some_and(|key| !key.trim().is_empty())
    {
        println!("note: --api-key still supplies a key for this run");
    }
    Ok(())
}

/// Placeholder provider for an interactive start without credentials. The
/// TUI blocks normal prompts until `/login`, so this is only a safety net.
pub(crate) fn no_credentials_provider() -> Arc<dyn Provider> {
    struct NoCredentials;

    impl Provider for NoCredentials {
        fn name(&self) -> &'static str {
            "none"
        }

        fn stream<'a>(
            &'a self,
            _request: llm::ChatRequest<'a>,
            _sink: tokio::sync::mpsc::UnboundedSender<llm::StreamEvent>,
            _cancel: llm::Cancel,
        ) -> llm::BoxFuture<'a, std::result::Result<(), llm::Error>> {
            Box::pin(async { Err(llm::Error::Stream("not logged in — type /login".into())) })
        }
    }

    Arc::new(NoCredentials)
}
