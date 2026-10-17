//! Hand-rolled argument parsing. No clap: the surface is small and the
//! philosophy says don't pull in what you don't need.

use llm::Effort;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Args {
    /// A subcommand (`login` / `logout`) instead of an agent turn.
    pub command: Option<Command>,
    pub prompt: Option<String>,
    pub print: bool,
    pub json: bool,
    pub continue_: bool,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub no_thinking: bool,
    pub effort: Option<Effort>,
    pub tools: Option<Vec<String>>,
    pub help: bool,
    pub version: bool,
}

impl Args {
    pub fn mode(&self) -> Mode {
        if self.json {
            Mode::Json
        } else if self.print || self.prompt.is_some() {
            Mode::Print
        } else {
            Mode::Interactive
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Interactive,
    Print,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Login,
    Logout,
}

pub fn parse<I: IntoIterator<Item = String>>(argv: I) -> Result<Args, String> {
    let mut args = Args::default();
    let mut positionals: Vec<String> = Vec::new();
    let mut iter = argv.into_iter();

    let value = |iter: &mut dyn Iterator<Item = String>, flag: &str| -> Result<String, String> {
        iter.next().ok_or_else(|| format!("{flag} needs a value"))
    };

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-p" | "--print" => args.print = true,
            "--json" => args.json = true,
            "-c" | "--continue" => args.continue_ = true,
            "--no-thinking" => args.no_thinking = true,
            "-h" | "--help" => args.help = true,
            "-V" | "--version" => args.version = true,
            "--model" => args.model = Some(value(&mut iter, "--model")?),
            "--provider" => {
                let provider = value(&mut iter, "--provider")?;
                if provider != "deepseek" && provider != "openai" {
                    return Err(format!(
                        "unknown provider: {provider} (available: deepseek, openai)"
                    ));
                }
                args.provider = Some(provider);
            }
            "--base-url" => args.base_url = Some(value(&mut iter, "--base-url")?),
            "--api-key" => args.api_key = Some(value(&mut iter, "--api-key")?),
            "--effort" => {
                let effort = value(&mut iter, "--effort")?;
                args.effort = Some(match effort.as_str() {
                    "low" => Effort::Low,
                    "high" => Effort::High,
                    "max" => Effort::Max,
                    other => return Err(format!("unknown effort: {other} (low|high|max)")),
                });
            }
            "--tools" => {
                let list = value(&mut iter, "--tools")?;
                let names: Vec<String> = list
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect();
                if names.is_empty() {
                    return Err("--tools needs at least one tool name".into());
                }
                args.tools = Some(names);
            }
            other if other.starts_with('-') && other.len() > 1 => {
                return Err(format!("unknown option: {other}"));
            }
            _ => {
                // The first positional may be a subcommand, never a prompt.
                if args.command.is_none() && positionals.is_empty() {
                    match arg.as_str() {
                        "login" => {
                            args.command = Some(Command::Login);
                            continue;
                        }
                        "logout" => {
                            args.command = Some(Command::Logout);
                            continue;
                        }
                        _ => {}
                    }
                }
                positionals.push(arg);
            }
        }
    }

    if args.command.is_some() {
        if !positionals.is_empty() {
            return Err("login/logout take no prompt".into());
        }
        if args.json || args.print {
            return Err("login/logout cannot be combined with --json or -p".into());
        }
        return Ok(args);
    }

    if !positionals.is_empty() {
        args.prompt = Some(positionals.join(" "));
    }
    if args.json && !args.print && args.prompt.is_none() {
        return Err("--json needs a prompt: pi-rs --json -p \"...\"".into());
    }
    Ok(args)
}

pub const USAGE: &str = "\
pi-rs — a minimal coding agent (DeepSeek by default)

USAGE:
    pi-rs [OPTIONS] [PROMPT]
    pi-rs login [--api-key <key>] [--provider <name>] [--base-url <url>]
    pi-rs logout

COMMANDS:
    login                    verify an API key and save it to auth.json
                             (reads $DEEPSEEK_API_KEY, stdin, or a hidden prompt)
    logout                   remove the saved API key

MODES:
    (no prompt)              interactive terminal UI
    PROMPT, -p               run one prompt, stream the answer, exit
    --json                   emit agent events as JSONL on stdout

OPTIONS:
    -p, --print              run one prompt (reads stdin when empty)
    -c, --continue           continue the most recent session here
        --model <id>         model (default: deepseek-flash)
        --provider <name>    deepseek (default) | openai
        --base-url <url>     override the API base URL
        --api-key <key>      API key (default: $DEEPSEEK_API_KEY)
        --no-thinking        disable DeepSeek thinking mode
        --effort <level>     reasoning effort: low | high | max
        --tools <list>       restrict tools, e.g. --tools read,bash
    -h, --help               show this help
    -V, --version            show the version

ENV:
    DEEPSEEK_API_KEY         API key for the default provider
    OPENAI_API_KEY           API key for --provider openai
    PI_RS_HOME               data directory (default: ~/.pi-rs)

AUTH:
    ~/.pi-rs/auth.json       written by `pi-rs login` / `/login` (mode 0600);
                             --api-key and environment variables win over it
";

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ok(args: &[&str]) -> Args {
        parse(args.iter().map(|s| s.to_string())).unwrap()
    }

    fn parse_err(args: &[&str]) -> String {
        parse(args.iter().map(|s| s.to_string())).unwrap_err()
    }

    #[test]
    fn modes() {
        assert_eq!(parse_ok(&[]).mode(), Mode::Interactive);
        assert_eq!(parse_ok(&["-p"]).mode(), Mode::Print);
        assert_eq!(parse_ok(&["hello", "world"]).mode(), Mode::Print);
        assert_eq!(parse_ok(&["--json", "-p"]).mode(), Mode::Json);
        assert!(!parse_err(&["--json"]).is_empty());
    }

    #[test]
    fn positionals_join_into_a_prompt() {
        assert_eq!(
            parse_ok(&["fix", "the", "bug"]).prompt.as_deref(),
            Some("fix the bug")
        );
    }

    #[test]
    fn options_with_values() {
        let args = parse_ok(&[
            "--model",
            "deepseek-v4-pro",
            "--effort",
            "max",
            "--tools",
            "read, bash",
            "--api-key",
            "sk-x",
        ]);
        assert_eq!(args.model.as_deref(), Some("deepseek-v4-pro"));
        assert_eq!(args.effort, Some(Effort::Max));
        assert_eq!(
            args.tools,
            Some(vec!["read".to_string(), "bash".to_string()])
        );
        assert_eq!(args.api_key.as_deref(), Some("sk-x"));
    }

    #[test]
    fn rejects_bad_options() {
        assert!(parse_err(&["--nope"]).contains("unknown option"));
        assert!(parse_err(&["--model"]).contains("--model"));
        assert!(parse_err(&["--effort", "turbo"]).contains("effort"));
        assert!(parse_err(&["--provider", "gemini"]).contains("provider"));
    }

    #[test]
    fn login_and_logout_are_commands() {
        let login = parse_ok(&["login", "--api-key", "sk-x"]);
        assert_eq!(login.command, Some(Command::Login));
        assert_eq!(login.api_key.as_deref(), Some("sk-x"));
        assert_eq!(login.prompt, None);

        let logout = parse_ok(&["logout", "--provider", "openai"]);
        assert_eq!(logout.command, Some(Command::Logout));
        assert_eq!(logout.provider.as_deref(), Some("openai"));

        // Anything after the command is a prompt, which login does not take.
        assert!(parse_err(&["login", "now"]).contains("no prompt"));
        assert!(parse_err(&["logout", "-p"]).contains("cannot be combined"));
        // A later "login" is just part of the prompt.
        let prompt = parse_ok(&["please", "login"]);
        assert_eq!(prompt.command, None);
        assert_eq!(prompt.prompt.as_deref(), Some("please login"));
    }
}
