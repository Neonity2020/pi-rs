//! The three run modes: interactive TUI, print, and JSON.
//! All of them drive the same `Agent` with the same tools, prompt and
//! session; only the event sink and the loop differ.

use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use agent::{Agent, Cancel, Message, Outcome, Provider, Tool};
use anyhow::Context;
use llm::Model;
use serde_json::json;
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::sync::Mutex;

use crate::sinks::{JsonSink, PrintSink, Shared};
use crate::stats::format_tokens;
use crate::ui::{self, Action, App, UiEvent};
use crate::{auth, cli, login};
use ui::crossterm::event::MouseEventKind;

type Tools = Vec<Arc<dyn Tool>>;

/// Everything one turn needs; the three modes differ only in how they drive
/// the agent and where the events go.
pub struct Turn {
    pub provider: Arc<dyn Provider>,
    pub model: Model,
    pub system: String,
    pub tools: Tools,
    pub shared: Shared,
    pub messages: Vec<Message>,
}

/// Cancel on Ctrl+C; a second Ctrl+C exits immediately.
fn spawn_ctrl_c(cancel: Cancel) {
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            cancel.cancel();
            let _ = tokio::signal::ctrl_c().await;
            std::process::exit(130);
        }
    });
}

fn summary_line(shared: &Shared) -> String {
    let stats = shared.stats.lock().unwrap();
    let session = shared.session.lock().unwrap();
    format!(
        "— done · ↑{} ↓{} · cached {} · {} · {}",
        format_tokens(stats.usage.input_tokens),
        format_tokens(stats.usage.output_tokens),
        format_tokens(stats.usage.cached_tokens),
        stats.cost_display(&shared.model),
        session.path.display()
    )
}

fn build_agent(
    provider: Arc<dyn Provider>,
    model: &Model,
    system: String,
    tools: Tools,
    messages: Vec<Message>,
) -> Agent {
    Agent::new(provider, model.clone(), system, tools, messages)
}

pub async fn run_print(turn: Turn, prompt: String) -> anyhow::Result<()> {
    let Turn {
        provider,
        model,
        system,
        tools,
        shared,
        messages,
    } = turn;
    let mut agent = build_agent(provider, &model, system, tools, messages);
    shared.session.lock().unwrap().append_user(&prompt)?;

    let cancel = Cancel::new();
    spawn_ctrl_c(cancel.clone());

    let mut sink = PrintSink::new(shared.clone());
    match agent.run_turn(prompt, &mut sink, cancel).await {
        Ok(Outcome::Completed) => {
            if sink.wrote_text {
                println!();
            }
            eprintln!("{}", summary_line(&shared));
            Ok(())
        }
        Ok(Outcome::Aborted) => {
            if sink.wrote_text {
                println!();
            }
            eprintln!("interrupted");
            std::process::exit(130);
        }
        Err(error) => {
            if sink.wrote_text {
                println!();
            }
            Err(anyhow::anyhow!(error))
        }
    }
}

pub async fn run_json(turn: Turn, prompt: String) -> anyhow::Result<()> {
    let Turn {
        provider,
        model,
        system,
        tools,
        shared,
        messages,
    } = turn;
    let mut agent = build_agent(provider, &model, system, tools, messages);
    shared.session.lock().unwrap().append_user(&prompt)?;

    println!(
        "{}",
        json!({
            "type": "session",
            "id": shared.session.lock().unwrap().id,
            "file": shared.session.lock().unwrap().path,
            "model": model.id,
        })
    );
    println!("{}", json!({ "type": "user", "text": prompt }));

    let cancel = Cancel::new();
    spawn_ctrl_c(cancel.clone());

    let mut sink = JsonSink {
        shared: shared.clone(),
    };
    let outcome = agent.run_turn(prompt, &mut sink, cancel).await;
    let failed = outcome.is_err();
    let stats = shared.stats.lock().unwrap();
    let result = match outcome {
        Ok(Outcome::Completed) => json!({
            "type": "result",
            "outcome": "completed",
            "usage": stats.usage,
            "cost_usd": stats.cost_usd,
        }),
        Ok(Outcome::Aborted) => json!({ "type": "result", "outcome": "aborted" }),
        Err(error) => json!({ "type": "result", "outcome": "error", "message": error.to_string() }),
    };
    println!("{}", result);
    if failed {
        std::process::exit(1);
    }
    Ok(())
}

pub async fn run_interactive(
    turn: Turn,
    resumed: Option<usize>,
    logged_in: bool,
    args: cli::Args,
) -> anyhow::Result<()> {
    let Turn {
        provider,
        model,
        system,
        tools,
        shared,
        messages,
    } = turn;
    let agent = Arc::new(Mutex::new(build_agent(
        provider, &model, system, tools, messages,
    )));
    let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
    let (key_tx, mut key_rx) = mpsc::unbounded_channel::<InputMsg>();
    std::thread::spawn(move || loop {
        match ui::crossterm::event::read() {
            Ok(ui::crossterm::event::Event::Key(key)) => {
                if key_tx.send(InputMsg::Key(key)).is_err() {
                    break;
                }
            }
            Ok(ui::crossterm::event::Event::Mouse(mouse)) => {
                let delta = match mouse.kind {
                    MouseEventKind::ScrollUp => -1,
                    MouseEventKind::ScrollDown => 1,
                    _ => continue,
                };
                if key_tx.send(InputMsg::Scroll(delta)).is_err() {
                    break;
                }
            }
            Ok(ui::crossterm::event::Event::Resize(..)) => {
                if key_tx.send(InputMsg::Resize).is_err() {
                    break;
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    });

    let mut app = App::new(shared.status_info());
    app.logged_in = logged_in;
    if let Some(count) = resumed {
        app.apply(UiEvent::Info(format!("resumed session ({count} messages)")));
    }
    let cancel_cell = Arc::new(StdMutex::new(Cancel::new()));

    let mut terminal = TerminalGuard::enter()?;
    let mut ticker = tokio::time::interval(Duration::from_millis(120));
    let mut dirty = true;

    loop {
        tokio::select! {
            message = key_rx.recv() => {
                let Some(message) = message else { break };
                match message {
                    InputMsg::Resize => dirty = true,
                    InputMsg::Scroll(delta) => {
                        app.on_scroll(delta);
                        dirty = true;
                    }
                    InputMsg::Key(key) => match app.on_key(key) {
                        Action::Submit(text) => {
                            match slash_command(&text) {
                                Some(Slash::Login) => begin_login(&mut app, &args, &ui_tx),
                                Some(Slash::Logout) => {
                                    logout(&mut app, &args, &agent, &ui_tx).await
                                }
                                Some(Slash::Help) => {
                                    let _ = ui_tx.send(UiEvent::Info(HELP.into()));
                                }
                                Some(Slash::Unknown(name)) => {
                                    let _ = ui_tx.send(UiEvent::Error(format!(
                                        "unknown command: {name} — try /login, /logout, /help"
                                    )));
                                }
                                None if !app.logged_in => {
                                    let _ = ui_tx.send(UiEvent::Info(
                                        "not logged in — type /login to sign in with DeepSeek, or run `pi login`"
                                            .into(),
                                    ));
                                }
                                None => {
                                    let cancel = Cancel::new();
                                    *cancel_cell.lock().unwrap() = cancel.clone();
                                    spawn_turn(
                                        agent.clone(),
                                        shared.clone(),
                                        ui_tx.clone(),
                                        text,
                                        cancel,
                                    );
                                }
                            }
                            dirty = true;
                        }
                        Action::Login(key) => {
                            start_login(&mut app, &args, &agent, &ui_tx, key);
                            dirty = true;
                        }
                        Action::Interrupt => {
                            cancel_cell.lock().unwrap().cancel();
                        }
                        Action::Quit => break,
                        Action::None => dirty = true,
                    },
                }
            }
            event = ui_rx.recv() => {
                if let Some(event) = event {
                    app.apply(event);
                    dirty = true;
                }
            }
            _ = ticker.tick() => {
                if app.busy {
                    app.tick();
                    dirty = true;
                }
            }
        }
        if dirty {
            terminal
                .terminal
                .draw(|frame| ui::tui_render(&mut app, frame))
                .context("draw terminal")?;
            dirty = false;
        }
    }

    drop(terminal);
    eprintln!("session: {}", shared.session.lock().unwrap().path.display());
    Ok(())
}

const HELP: &str = "/login   sign in and save your API key to auth.json\n/logout  remove the saved API key\n/help    show this";

#[derive(Debug, PartialEq, Eq)]
enum Slash {
    Login,
    Logout,
    Help,
    Unknown(String),
}

/// `/login` etc. only count when they are the whole first token.
fn slash_command(text: &str) -> Option<Slash> {
    let command = text.split_whitespace().next()?;
    if !command.starts_with('/') {
        return None;
    }
    Some(match command {
        "/login" => Slash::Login,
        "/logout" => Slash::Logout,
        "/help" => Slash::Help,
        other => Slash::Unknown(other.to_string()),
    })
}

fn begin_login(app: &mut App, args: &cli::Args, ui: &UnboundedSender<UiEvent>) {
    let provider = crate::provider_name(args);
    let _ = ui.send(UiEvent::Info(format!(
        "sign in to {} — get a key at {}",
        login::label(provider),
        login::manage_url(provider)
    )));
    app.begin_login(login::label(provider));
}

/// Verify the key off the UI thread; on success the agent keeps its history
/// and simply gets a new provider.
fn start_login(
    app: &mut App,
    args: &cli::Args,
    agent: &Arc<Mutex<Agent>>,
    ui: &UnboundedSender<UiEvent>,
    key: String,
) {
    let opts = match login::options(args) {
        Ok(opts) => opts,
        Err(err) => {
            app.login = None;
            let _ = ui.send(UiEvent::Error(format!("login failed: {err}")));
            return;
        }
    };
    if let Some(prompt) = app.login.as_mut() {
        prompt.busy = true;
    }
    let mut next = args.clone();
    next.api_key = Some(key.clone());
    let agent = agent.clone();
    let ui = ui.clone();
    tokio::spawn(async move {
        let result = async {
            let path = login::verify_and_save(&opts, &key).await?;
            let provider = crate::build_provider(&next, &key)?;
            agent.lock().await.set_provider(provider);
            Ok::<_, anyhow::Error>(path)
        }
        .await;
        let (ok, message) = match result {
            Ok(path) => (
                true,
                format!(
                    "logged in to {} · saved to {}",
                    opts.provider,
                    path.display()
                ),
            ),
            Err(err) => (false, format!("login failed: {err}")),
        };
        let _ = ui.send(UiEvent::LoginDone { ok, message });
    });
}

/// `/logout` removes the saved key. A key from the environment or
/// `--api-key` keeps working, so only replace the provider when none is left.
async fn logout(
    app: &mut App,
    args: &cli::Args,
    agent: &Arc<Mutex<Agent>>,
    ui: &UnboundedSender<UiEvent>,
) {
    let provider = crate::provider_name(args).to_string();
    match auth::AuthFile::remove(&provider) {
        Ok(removed) => {
            if crate::resolve_api_key(args).ok().flatten().is_some() {
                app.logged_in = true;
                let _ = ui.send(UiEvent::Info(format!(
                    "removed saved credentials for {provider}; a key from the environment or --api-key is still in use"
                )));
            } else {
                agent
                    .lock()
                    .await
                    .set_provider(crate::no_credentials_provider());
                app.logged_in = false;
                let message = if removed {
                    format!("logged out of {provider}")
                } else {
                    format!("no saved credentials for {provider}")
                };
                let _ = ui.send(UiEvent::Info(message));
            }
        }
        Err(err) => {
            let _ = ui.send(UiEvent::Error(format!("logout failed: {err}")));
        }
    }
}

fn spawn_turn(
    agent: Arc<Mutex<Agent>>,
    shared: Shared,
    ui: UnboundedSender<UiEvent>,
    text: String,
    cancel: Cancel,
) {
    let _ = ui.send(UiEvent::User(text.clone()));
    let _ = shared.session.lock().unwrap().append_user(&text);
    let _ = ui.send(UiEvent::Busy(true));

    tokio::spawn(async move {
        let mut sink = ui::TuiSink {
            shared: shared.clone(),
            ui: ui.clone(),
        };
        let result = {
            let mut agent = agent.lock().await;
            agent.run_turn(text, &mut sink, cancel).await
        };
        match result {
            Ok(Outcome::Completed) => {}
            Ok(Outcome::Aborted) => {
                let _ = ui.send(UiEvent::Info("interrupted".into()));
            }
            Err(error) => {
                let _ = ui.send(UiEvent::Error(error.to_string()));
            }
        }
        let _ = ui.send(UiEvent::Status(shared.status_info()));
        let _ = ui.send(UiEvent::Busy(false));
    });
}

enum InputMsg {
    Key(ui::crossterm::event::KeyEvent),
    Scroll(i32),
    Resize,
}

/// Raw mode + alternate screen; restores the terminal on drop.
struct TerminalGuard {
    terminal: ui::ratatui::Terminal<ui::ratatui::backend::CrosstermBackend<std::io::Stdout>>,
}

impl TerminalGuard {
    fn enter() -> anyhow::Result<Self> {
        use ui::crossterm::event::EnableMouseCapture;
        use ui::crossterm::terminal as ct;
        ct::enable_raw_mode().context("enable raw mode")?;
        let mut stdout = std::io::stdout();
        ui::crossterm::execute!(stdout, ct::EnterAlternateScreen, EnableMouseCapture)
            .context("enter alternate screen")?;
        let backend = ui::ratatui::backend::CrosstermBackend::new(stdout);
        let terminal = ui::ratatui::Terminal::new(backend).context("create terminal")?;
        Ok(Self { terminal })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        use ui::crossterm::event::DisableMouseCapture;
        use ui::crossterm::terminal as ct;
        let _ = ct::disable_raw_mode();
        let _ = ui::crossterm::execute!(
            self.terminal.backend_mut(),
            DisableMouseCapture,
            ct::LeaveAlternateScreen
        );
        let _ = self.terminal.show_cursor();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slash_commands_are_the_first_token() {
        assert_eq!(slash_command("/login"), Some(Slash::Login));
        assert_eq!(slash_command("  /logout  "), Some(Slash::Logout));
        assert_eq!(slash_command("/help me"), Some(Slash::Help));
        assert_eq!(
            slash_command("/nope"),
            Some(Slash::Unknown("/nope".to_string()))
        );
        assert_eq!(slash_command("please /login"), None);
        assert_eq!(slash_command("hello"), None);
    }
}
