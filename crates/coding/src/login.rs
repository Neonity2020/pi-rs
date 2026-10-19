//! `pi-rs login` / `/login`: take an API key, verify it, save `auth.json`.
//!
//! The verification request goes through the provider layer
//! (`llm::openai::verify_api_key`); everything terminal-related stays here.

use std::io::{IsTerminal, Write};
use std::path::PathBuf;

use anyhow::{bail, Result};
use llm::Error as LlmError;

use crate::auth;
use crate::cli;
use crate::ui::crossterm;

/// Everything a login needs, derived from the CLI arguments.
#[derive(Debug, Clone)]
pub struct Options {
    pub provider: String,
    /// Endpoint the key is verified against.
    pub base_url: String,
    /// Value persisted in auth.json (an explicit `--base-url`, or the one
    /// already saved for this provider).
    pub store_base_url: Option<String>,
    /// Key supplied out of band (`--api-key`, then the provider's env var).
    pub key: Option<String>,
}

pub fn options(args: &cli::Args) -> Result<Options> {
    let provider = crate::provider_name(args).to_string();
    let existing = auth::AuthFile::credential(&provider)?;
    let stored_base = existing.as_ref().and_then(|c| c.base_url.clone());
    let base_url = args
        .base_url
        .clone()
        .or_else(|| stored_base.clone())
        .unwrap_or_else(|| crate::default_base_url(&provider).to_string());
    let store_base_url = args.base_url.clone().or(stored_base);
    Ok(Options {
        provider,
        base_url,
        store_base_url,
        key: args
            .api_key
            .clone()
            .filter(|key| !key.trim().is_empty())
            .or_else(|| crate::env_api_key(crate::provider_name(args))),
    })
}

/// CLI entry point: resolve a key, verify it, write auth.json, report.
pub async fn run(args: &cli::Args) -> Result<()> {
    let opts = options(args)?;
    let key = match opts.key.clone() {
        Some(key) => key,
        None => read_secret(&format!(
            "{} API key (input hidden): ",
            label(&opts.provider)
        ))?,
    };
    let key = key.trim().to_string();
    if key.is_empty() {
        bail!("no API key given");
    }
    let path = verify_and_save(&opts, &key).await?;
    println!(
        "logged in to {} · saved to {}",
        opts.provider,
        path.display()
    );
    println!("  manage keys: {}", manage_url(&opts.provider));
    Ok(())
}

/// Verify first, save second: a rejected key never reaches auth.json.
pub async fn verify_and_save(opts: &Options, key: &str) -> Result<PathBuf> {
    llm::openai::verify_api_key(&opts.base_url, key)
        .await
        .map_err(|err| match &err {
            LlmError::Api { status, .. } if *status == 401 || *status == 403 => {
                anyhow::anyhow!(
                    "the API rejected this key (status {status}) — create a new one at {}",
                    manage_url(&opts.provider)
                )
            }
            LlmError::Http(_) => {
                anyhow::anyhow!("could not reach {}: {err}", opts.base_url)
            }
            _ => anyhow::anyhow!("{err}"),
        })?;
    auth::AuthFile::save(&opts.provider, key, opts.store_base_url.as_deref())
}

/// Read a secret from a pipe, or from a TTY without echoing it.
pub fn read_secret(prompt: &str) -> Result<String> {
    use crossterm::event::{Event, KeyCode, KeyModifiers};
    use crossterm::terminal;

    if !std::io::stdin().is_terminal() {
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .map_err(|err| anyhow::anyhow!("read API key from stdin: {err}"))?;
        return Ok(line.trim().to_string());
    }

    eprint!("{prompt}");
    let _ = std::io::stderr().flush();
    terminal::enable_raw_mode()?;
    let result = (|| -> Result<String> {
        let mut buffer = String::new();
        loop {
            match crossterm::event::read()? {
                Event::Key(key) => match key.code {
                    KeyCode::Enter => break,
                    KeyCode::Esc => bail!("login cancelled"),
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        bail!("login cancelled")
                    }
                    KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        buffer.push(c);
                        eprint!("•");
                        let _ = std::io::stderr().flush();
                    }
                    KeyCode::Backspace if !buffer.is_empty() => {
                        buffer.pop();
                        eprint!("\x08 \x08");
                        let _ = std::io::stderr().flush();
                    }
                    _ => {}
                },
                Event::Paste(text) => {
                    buffer.push_str(&text);
                    eprint!("{}", "•".repeat(text.chars().count()));
                    let _ = std::io::stderr().flush();
                }
                _ => {}
            }
        }
        Ok(buffer)
    })();
    let _ = terminal::disable_raw_mode();
    eprintln!();
    result
}

pub fn label(provider: &str) -> &str {
    match provider {
        "deepseek" => "DeepSeek",
        "openai" => "OpenAI",
        "minimax-cn" => "MiniMax CN",
        other => other,
    }
}

pub fn manage_url(provider: &str) -> &'static str {
    match provider {
        "openai" => "https://platform.openai.com/api-keys",
        "minimax-cn" => "https://platform.minimax.cn/console/plan",
        _ => "https://platform.deepseek.com/api_keys",
    }
}
