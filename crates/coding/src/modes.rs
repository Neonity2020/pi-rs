//! The three run modes: interactive TUI, print, and JSON.
//! All of them drive the same `Agent` with the same tools, prompt and
//! session; only the event sink and the loop differ.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use agent::{Agent, Cancel, CompactionMode, Image, Message, Outcome, Provider, Tool};
use anyhow::Context;
use llm::Model;
use serde_json::json;
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::sync::Mutex;

use crate::sinks::{JsonSink, PrintSink, Shared};
use crate::stats::format_tokens;
use crate::ui::{self, Action, App, ImageAttachment, Submission, UiEvent};
use crate::{auth, cli, history, login, memory, util};
use ui::crossterm::event::{MouseButton, MouseEventKind};

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
    /// Summary from the latest compaction, so the agent updates it instead of
    /// re-summarizing the whole prefix.
    pub summary: Option<String>,
    /// Working directory, for scoping long-term memory commands.
    pub cwd: PathBuf,
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
    summary: Option<String>,
) -> Agent {
    let mut agent = Agent::new(provider, model.clone(), system, tools, messages);
    agent.set_previous_summary(summary);
    agent
}

pub async fn run_print(turn: Turn, prompt: String) -> anyhow::Result<()> {
    let Turn {
        provider,
        model,
        system,
        tools,
        shared,
        messages,
        summary,
        cwd: _,
    } = turn;
    let message = Message::user(prompt);
    let mut agent = build_agent(provider, &model, system, tools, messages, summary);
    shared.session.lock().unwrap().append_message(&message)?;

    let cancel = Cancel::new();
    spawn_ctrl_c(cancel.clone());

    let mut sink = PrintSink::new(shared.clone());
    match agent.run_turn(message, &mut sink, cancel).await {
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
        summary,
        cwd: _,
    } = turn;
    let message = Message::user(prompt);
    let mut agent = build_agent(provider, &model, system, tools, messages, summary);
    shared.session.lock().unwrap().append_message(&message)?;

    println!(
        "{}",
        json!({
            "type": "session",
            "id": shared.session.lock().unwrap().id,
            "file": shared.session.lock().unwrap().path,
            "model": model.id,
        })
    );
    println!("{}", json!({ "type": "user", "text": message.text() }));

    let cancel = Cancel::new();
    spawn_ctrl_c(cancel.clone());

    let mut sink = JsonSink {
        shared: shared.clone(),
    };
    let outcome = agent.run_turn(message, &mut sink, cancel).await;
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
        summary,
        cwd,
    } = turn;
    let agent = Arc::new(Mutex::new(build_agent(
        provider, &model, system, tools, messages, summary,
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
                let message = match mouse.kind {
                    MouseEventKind::ScrollUp => InputMsg::Scroll(-1),
                    MouseEventKind::ScrollDown => InputMsg::Scroll(1),
                    MouseEventKind::Down(MouseButton::Left) => InputMsg::Click {
                        column: mouse.column,
                        row: mouse.row,
                    },
                    _ => continue,
                };
                if key_tx.send(message).is_err() {
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
    // Up/Down recall prompts from this and earlier runs in this directory.
    app.input.seed_history(history::load());
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
                    InputMsg::Click { column, row } => {
                        if app.on_click(column, row) {
                            dirty = true;
                        }
                    }
                    InputMsg::Key(key) => match app.on_key(key) {
                        Action::Submit(mut submission) => {
                            // Persist the prompt even if it is a slash command:
                            // recalling `/login` later is useful too.
                            history::append(&submission.text);
                            // Slash commands are text-only. Keep any attached
                            // images in the input box for the next prompt.
                            let command = slash_command(&submission.text);
                            if command.is_some() {
                                for image in std::mem::take(&mut submission.images) {
                                    let _ = ui_tx.send(UiEvent::ImageAttached(image));
                                }
                            }
                            match command {
                                Some(Slash::Login) => begin_login(&mut app, &args, &ui_tx),
                                Some(Slash::Logout) => {
                                    logout(&mut app, &args, &agent, &ui_tx).await
                                }
                                Some(Slash::Help) => {
                                    let _ = ui_tx.send(UiEvent::Info(HELP.into()));
                                }
                                Some(Slash::Remember { scope, text }) => {
                                    remember_memory(&cwd, scope, &text, &ui_tx);
                                }
                                Some(Slash::Forget { scope, text }) => {
                                    forget_memory(&cwd, scope, &text, &ui_tx);
                                }
                                Some(Slash::Memory) => {
                                    let _ = ui_tx.send(UiEvent::Info(memory::describe(
                                        &util::home_dir(),
                                        &cwd,
                                    )));
                                }
                                Some(Slash::Compact) => {
                                    if app.busy {
                                        let _ = ui_tx.send(UiEvent::Error(
                                            "wait for the current turn to finish before /compact"
                                                .into(),
                                        ));
                                    } else {
                                        app.busy = true;
                                        let _ = ui_tx.send(UiEvent::Busy(true));
                                        let cancel = Cancel::new();
                                        *cancel_cell.lock().unwrap() = cancel.clone();
                                        let agent = agent.clone();
                                        let shared = shared.clone();
                                        let ui = ui_tx.clone();
                                        tokio::spawn(async move {
                                            let mut sink = ui::TuiSink {
                                                shared,
                                                ui: ui.clone(),
                                            };
                                            let mut agent = agent.lock().await;
                                            match agent
                                                .compact(
                                                    CompactionMode::Manual,
                                                    &mut sink,
                                                    cancel,
                                                )
                                                .await
                                            {
                                                Ok(Some(_)) => {}
                                                Ok(None) => {
                                                    let _ = ui.send(UiEvent::Info(
                                                        "nothing to compact — the session is already small"
                                                            .into(),
                                                    ));
                                                }
                                                Err(agent::Error::Cancelled) => {
                                                    let _ = ui
                                                        .send(UiEvent::Info("compaction cancelled".into()));
                                                }
                                                Err(error) => {
                                                    let _ = ui.send(UiEvent::Error(format!(
                                                        "compact failed: {error}"
                                                    )));
                                                }
                                            }
                                            let _ = ui.send(UiEvent::Busy(false));
                                        });
                                    }
                                }
                                Some(Slash::Unknown(name)) => {
                                    let _ = ui_tx.send(UiEvent::Error(format!(
                                        "unknown command: {name} — try /login, /logout, /compact, /remember, /forget, /memory, /help"
                                    )));
                                }
                                None if !app.logged_in => {
                                    // Give the images back instead of silently
                                    // dropping what may be a 2 MB screenshot.
                                    for image in submission.images {
                                        let _ = ui_tx.send(UiEvent::ImageAttached(image));
                                    }
                                    let _ = ui_tx.send(UiEvent::Info(
                                        "not logged in — type /login to sign in with DeepSeek, or run `pi-rs login`"
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
                                        submission,
                                        cancel,
                                    );
                                }
                            }
                            dirty = true;
                        }
                        Action::PasteImage => {
                            paste_image(&ui_tx);
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

const HELP: &str = "/login            sign in and save your API key to auth.json\n/logout           remove the saved API key\n/compact          summarize older messages to free context\n/remember <text>  add a note to this project's long-term memory\n/remember global  add a note for all projects\n/forget <text>    remove matching notes from this project's memory\n/memory           show memory file paths and counts\n/help             show this\n\nCtrl+V   attach an image from the clipboard (Esc removes the last one)\nCtrl+T   expand or collapse the agent's thinking";

#[derive(Debug, PartialEq, Eq)]
enum Slash {
    Login,
    Logout,
    Compact,
    Remember { scope: memory::Scope, text: String },
    Forget { scope: memory::Scope, text: String },
    Memory,
    Help,
    Unknown(String),
}

/// `/login` etc. only count when they are the whole first token.
fn slash_command(text: &str) -> Option<Slash> {
    let text = text.trim();
    if !text.starts_with('/') {
        return None;
    }
    let (command, rest) = text
        .split_once(|c: char| c.is_whitespace())
        .unwrap_or((text, ""));
    let rest = rest.trim_start();
    Some(match command {
        "/login" => Slash::Login,
        "/logout" => Slash::Logout,
        "/compact" => Slash::Compact,
        "/remember" => {
            let (scope, text) = split_scope(rest);
            Slash::Remember {
                scope,
                text: text.to_string(),
            }
        }
        "/forget" => {
            let (scope, text) = split_scope(rest);
            Slash::Forget {
                scope,
                text: text.to_string(),
            }
        }
        "/memory" => Slash::Memory,
        "/help" => Slash::Help,
        other => Slash::Unknown(other.to_string()),
    })
}

/// `/remember global ...` writes to the global file; everything else is
/// project-scoped. Only a standalone `global` first word counts.
fn split_scope(rest: &str) -> (memory::Scope, &str) {
    if rest == "global" {
        return (memory::Scope::Global, "");
    }
    if let Some(after) = rest.strip_prefix("global") {
        if after.chars().next().is_some_and(char::is_whitespace) {
            return (memory::Scope::Global, after.trim_start());
        }
    }
    (memory::Scope::Project, rest)
}

fn remember_memory(
    cwd: &std::path::Path,
    scope: memory::Scope,
    text: &str,
    ui: &UnboundedSender<UiEvent>,
) {
    let home = util::home_dir();
    match memory::remember(&home, cwd, scope, text) {
        Ok(memory::RememberOutcome::Added) => {
            let path = memory::path(&home, cwd, scope);
            let _ = ui.send(UiEvent::Info(format!("remembered in {}", path.display())));
        }
        Ok(memory::RememberOutcome::Duplicate) => {
            let _ = ui.send(UiEvent::Info("already remembered".into()));
        }
        Err(error) => {
            let _ = ui.send(UiEvent::Error(format!("remember failed: {error}")));
        }
    }
}

fn forget_memory(
    cwd: &std::path::Path,
    scope: memory::Scope,
    text: &str,
    ui: &UnboundedSender<UiEvent>,
) {
    let home = util::home_dir();
    match memory::forget(&home, cwd, scope, text) {
        Ok(removed) if removed.is_empty() => {
            let path = memory::path(&home, cwd, scope);
            let _ = ui.send(UiEvent::Info(format!(
                "no matching memory in {}",
                path.display()
            )));
        }
        Ok(removed) => {
            let mut message = format!(
                "removed {} {} from {}",
                removed.len(),
                if removed.len() == 1 {
                    "entry"
                } else {
                    "entries"
                },
                memory::path(&home, cwd, scope).display()
            );
            for text in removed.iter().take(3) {
                message.push_str(&format!("\n- {text}"));
            }
            if removed.len() > 3 {
                message.push_str("\n…");
            }
            let _ = ui.send(UiEvent::Info(message));
        }
        Err(error) => {
            let _ = ui.send(UiEvent::Error(format!("forget failed: {error}")));
        }
    }
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
    submission: Submission,
    cancel: Cancel,
) {
    let Submission { text, images } = submission;
    let labels = images
        .iter()
        .map(ImageAttachment::label)
        .collect::<Vec<_>>();
    let message = Message::user_with_images(
        text,
        images
            .into_iter()
            .map(|image| Image::new(image.media_type, image.data))
            .collect(),
    );

    let _ = ui.send(UiEvent::User {
        text: message.text().to_string(),
        images: labels,
    });
    let _ = shared.session.lock().unwrap().append_message(&message);
    let _ = ui.send(UiEvent::Busy(true));

    tokio::spawn(async move {
        let mut sink = ui::TuiSink {
            shared: shared.clone(),
            ui: ui.clone(),
        };
        let result = {
            let mut agent = agent.lock().await;
            agent.run_turn(message, &mut sink, cancel).await
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

/// Ctrl+V. Clipboard access is synchronous and cheap (one platform process);
/// the UI is frozen for a few milliseconds, which is less surprising than
/// images appearing after the prompt was already sent.
fn paste_image(ui: &UnboundedSender<UiEvent>) {
    match crate::clipboard::read_image() {
        Ok(Some(image)) => {
            let _ = ui.send(UiEvent::ImageAttached(ImageAttachment {
                media_type: image.media_type.to_string(),
                data: image.data,
                bytes: image.bytes,
                width: image.width,
                height: image.height,
            }));
        }
        Ok(None) => {
            let _ = ui.send(UiEvent::Info(
                "clipboard has no image — copy a screenshot first".into(),
            ));
        }
        Err(error) => {
            let _ = ui.send(UiEvent::Error(format!("paste image: {error}")));
        }
    }
}

enum InputMsg {
    Key(ui::crossterm::event::KeyEvent),
    Scroll(i32),
    Click { column: u16, row: u16 },
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
        assert_eq!(slash_command("/compact"), Some(Slash::Compact));
        assert_eq!(slash_command("/memory"), Some(Slash::Memory));
        assert_eq!(
            slash_command("/remember  a note  "),
            Some(Slash::Remember {
                scope: memory::Scope::Project,
                text: "a note".into()
            })
        );
        assert_eq!(
            slash_command("/remember global cross-project"),
            Some(Slash::Remember {
                scope: memory::Scope::Global,
                text: "cross-project".into()
            })
        );
        assert_eq!(
            slash_command("/forget global old"),
            Some(Slash::Forget {
                scope: memory::Scope::Global,
                text: "old".into()
            })
        );
        assert_eq!(
            slash_command("/remember global"),
            Some(Slash::Remember {
                scope: memory::Scope::Global,
                text: "".into()
            })
        );
        assert_eq!(
            slash_command("/nope"),
            Some(Slash::Unknown("/nope".to_string()))
        );
        assert_eq!(slash_command("please /login"), None);
        assert_eq!(slash_command("hello"), None);
    }
}
