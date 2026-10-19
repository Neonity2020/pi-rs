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
use crate::{auth, cli, dream, history, login, memory, util};
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
        stats.cost_display(&shared.model()),
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
    let cancel = Cancel::new();
    shared.begin_turn(cancel.clone())?;
    shared.append_message(&message)?;
    spawn_ctrl_c(cancel.clone());

    let mut sink = PrintSink::new(shared.clone());
    let outcome = agent.run_turn(message, &mut sink, cancel).await;
    if sink.wrote_text {
        println!();
    }
    shared.persistence_result()?;
    match outcome {
        Ok(Outcome::Completed) => {
            eprintln!("{}", summary_line(&shared));
            Ok(())
        }
        Ok(Outcome::Aborted) => {
            eprintln!("interrupted");
            std::process::exit(130);
        }
        Err(error) => Err(anyhow::anyhow!(error)),
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

    println!(
        "{}",
        json!({
            "type": "session",
            "id": shared.session.lock().unwrap().id,
            "file": shared.session.lock().unwrap().path,
            "model": model.id,
        })
    );
    let outcome: anyhow::Result<Outcome> = async {
        let cancel = Cancel::new();
        shared.begin_turn(cancel.clone())?;
        shared.append_message(&message)?;
        println!("{}", json!({ "type": "user", "text": message.text() }));
        spawn_ctrl_c(cancel.clone());
        let mut sink = JsonSink {
            shared: shared.clone(),
        };
        let outcome = agent.run_turn(message, &mut sink, cancel).await;
        shared.persistence_result()?;
        Ok(outcome?)
    }
    .await;
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
    // Keep a snapshot of the file tools for experimental `/dream`. It runs
    // in an isolated agent so maintenance chatter never enters this history.
    let dream_tools = dream::tools(&tools);
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
                                    let _ = ui_tx.send(UiEvent::Info(ui::help_text()));
                                }
                                Some(Slash::Model { id: None }) => {
                                    let _ = ui_tx.send(UiEvent::Info(list_models(&args, &shared)));
                                }
                                Some(Slash::Model { id: Some(id) }) => {
                                    if app.busy {
                                        let _ = ui_tx.send(UiEvent::Error(
                                            "wait for the current turn to finish before /model"
                                                .into(),
                                        ));
                                    } else {
                                        switch_model(&args, &agent, &shared, &ui_tx, &id).await;
                                    }
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
                                Some(Slash::Lab) => {
                                    app.lab = !app.lab;
                                    let _ = ui_tx.send(UiEvent::Info(if app.lab {
                                        "lab mode on — experimental commands: /dream".into()
                                    } else {
                                        "lab mode off".into()
                                    }));
                                }
                                Some(Slash::Dream | Slash::Compact)
                                    if shared.persistence_result().is_err() => {
                                    let error = shared.persistence_result().unwrap_err();
                                    let _ = ui_tx.send(UiEvent::Error(error.to_string()));
                                }
                                Some(Slash::Dream) => {
                                    if !app.lab {
                                        let _ = ui_tx.send(UiEvent::Info(
                                            "dream is experimental — type /lab to enter lab mode"
                                                .into(),
                                        ));
                                    } else if app.busy {
                                        let _ = ui_tx.send(UiEvent::Error(
                                            "wait for the current turn to finish before /dream"
                                                .into(),
                                        ));
                                    } else if !app.logged_in {
                                        let _ = ui_tx.send(UiEvent::Info(
                                            "not logged in — /dream needs a model; type /login"
                                                .into(),
                                        ));
                                    } else if !dream::can_run(&dream_tools) {
                                        let _ = ui_tx.send(UiEvent::Error(
                                            "/dream needs the read and write tools".into(),
                                        ));
                                    } else {
                                        let home = util::home_dir();
                                        let before = memory::counts(&home, &cwd);
                                        if before.total() == 0 {
                                            let _ = ui_tx.send(UiEvent::Info(
                                                "long-term memory is empty — nothing to dream about"
                                                    .into(),
                                            ));
                                        } else {
                                            app.busy = true;
                                            let _ = ui_tx.send(UiEvent::Busy(true));
                                            let cancel = Cancel::new();
                                            *cancel_cell.lock().unwrap() = cancel.clone();
                                            dream::spawn(
                                                agent.clone(),
                                                dream_tools.clone(),
                                                shared.clone(),
                                                cwd.clone(),
                                                ui_tx.clone(),
                                                cancel,
                                                before,
                                            );
                                        }
                                    }
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
                                            if let Err(error) = shared.begin_turn(cancel.clone()) {
                                                let _ = ui.send(UiEvent::Error(error.to_string()));
                                                let _ = ui.send(UiEvent::Busy(false));
                                                return;
                                            }
                                            let mut sink = ui::TuiSink::persisting(
                                                shared.clone(),
                                                ui.clone(),
                                            );
                                            let mut agent = agent.lock().await;
                                            let outcome = agent
                                                .compact(
                                                    CompactionMode::Manual,
                                                    &mut sink,
                                                    cancel,
                                                )
                                                .await;
                                            let outcome = shared.persistence_result()
                                                .and_then(|_| outcome.map_err(anyhow::Error::from));
                                            match outcome {
                                                Ok(Some(_)) => {}
                                                Ok(None) => {
                                                    let _ = ui.send(UiEvent::Info(
                                                        "nothing to compact — the session is already small"
                                                            .into(),
                                                    ));
                                                }
                                                Err(error) if error.downcast_ref::<agent::Error>()
                                                    .is_some_and(|e| matches!(e, agent::Error::Cancelled)) => {
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
                                        "unknown command: {name} — type /help to list commands"
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
    shared.persistence_result()
}

#[derive(Debug, PartialEq, Eq)]
enum Slash {
    Login,
    Logout,
    Compact,
    /// `/model` lists the available models; `/model <id>` switches to one.
    Model {
        id: Option<String>,
    },
    Remember {
        scope: memory::Scope,
        text: String,
    },
    Forget {
        scope: memory::Scope,
        text: String,
    },
    Memory,
    Lab,
    Dream,
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
        "/model" => Slash::Model {
            id: (!rest.is_empty()).then(|| rest.to_string()),
        },
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
        "/lab" => Slash::Lab,
        "/dream" => Slash::Dream,
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

/// `/model` with no argument: list every model pi-rs knows, marking the
/// current one. Models whose provider has no credentials are listed too —
/// they are reachable once the key is there.
fn list_models(args: &cli::Args, shared: &Shared) -> String {
    let current = shared.model();
    let mut text = String::new();
    for model in llm::all_models() {
        let marker = if model.id == current.id { '*' } else { ' ' };
        let window = if model.context_window > 0 {
            format!("{} ctx", crate::stats::format_tokens(model.context_window))
        } else {
            "unknown ctx".to_string()
        };
        let current_provider = model.provider == current.provider;
        let suffix = if current_provider {
            String::new()
        } else {
            match crate::resolve_api_key_for(&model.provider, args) {
                Ok(Some(_)) => String::new(),
                _ => " (no key)".to_string(),
            }
        };
        text.push_str(&format!(
            "{marker} {:<22} {:<12} {window}{suffix}\n",
            model.id, model.provider
        ));
    }
    text.push_str("switch with /model <id>");
    text
}

/// `/model <id>`: switch models. The id may be an alias (`MiniMax-M3.1`) and
/// may name a different provider than the current one; that needs that
/// provider's own credentials, so the provider is rebuilt when it changes.
async fn switch_model(
    args: &cli::Args,
    agent: &Arc<Mutex<Agent>>,
    shared: &Shared,
    ui: &UnboundedSender<UiEvent>,
    id: &str,
) {
    let current = shared.model();
    let provider = llm::provider_of_model(id)
        .map(str::to_string)
        .unwrap_or_else(|| current.provider.clone());
    let model = llm::provider_model(&provider, id);

    if provider != current.provider {
        let key = match crate::resolve_api_key_for(&provider, args) {
            Ok(Some(key)) => key,
            Ok(None) => {
                let _ = ui.send(UiEvent::Error(format!(
                    "no API key for {provider} — run `pi-rs login --provider {provider}` or set {}",
                    crate::env_key_name(&provider)
                )));
                return;
            }
            Err(error) => {
                let _ = ui.send(UiEvent::Error(format!("model switch failed: {error}")));
                return;
            }
        };
        let next = match crate::build_provider_for(&provider, args, &key) {
            Ok(provider) => provider,
            Err(error) => {
                let _ = ui.send(UiEvent::Error(format!("model switch failed: {error}")));
                return;
            }
        };
        let mut agent = agent.lock().await;
        agent.set_provider(next);
        agent.set_model(model.clone());
    } else {
        agent.lock().await.set_model(model.clone());
    }

    shared.set_model(model.clone());
    let _ = ui.send(UiEvent::Info(format!("model: {id}")));
    let _ = ui.send(UiEvent::Status(shared.status_info()));
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

    if let Err(error) = shared
        .begin_turn(cancel.clone())
        .and_then(|_| shared.append_message(&message))
    {
        let _ = ui.send(UiEvent::Error(error.to_string()));
        let _ = ui.send(UiEvent::Busy(false));
        return;
    }
    let _ = ui.send(UiEvent::User {
        text: message.text().to_string(),
        images: labels,
    });
    let _ = ui.send(UiEvent::Busy(true));

    tokio::spawn(async move {
        let mut sink = ui::TuiSink::persisting(shared.clone(), ui.clone());
        let result = {
            let mut agent = agent.lock().await;
            agent.run_turn(message, &mut sink, cancel).await
        };
        let result = shared
            .persistence_result()
            .and_then(|_| result.map_err(anyhow::Error::from));
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

    #[tokio::test]
    async fn tui_rejects_a_prompt_that_cannot_be_saved() {
        let dir = crate::testutil::temp_dir("tui-user-persistence");
        let session = crate::session::Session::create_in(&dir, &dir, "m").unwrap();
        std::fs::remove_file(&session.path).unwrap();
        let shared = Shared::new(session, Model::unknown("mock", "m"));
        let provider = Arc::new(llm::mock::ScriptedProvider::new(vec![]));
        let agent = Arc::new(Mutex::new(Agent::new(
            provider.clone(),
            shared.model(),
            String::new(),
            vec![],
            vec![],
        )));
        let (tx, mut rx) = mpsc::unbounded_channel();
        spawn_turn(
            agent.clone(),
            shared.clone(),
            tx,
            Submission {
                text: "hello".into(),
                images: vec![],
            },
            Cancel::new(),
        );
        assert!(
            matches!(rx.recv().await, Some(UiEvent::Error(message)) if message.contains("session persistence failed"))
        );
        assert!(matches!(rx.recv().await, Some(UiEvent::Busy(false))));
        assert!(provider.observed().is_empty());
        assert!(agent.lock().await.messages().is_empty());
        assert!(shared.persistence_result().is_err());
    }

    #[tokio::test]
    async fn tui_reports_failed_tool_persistence_and_blocks_the_next_turn() {
        use agent::{BoxFuture, ToolResult};
        use llm::{mock::ScriptedProvider, StopReason, StreamEvent};

        struct RemoveSession(PathBuf);
        impl Tool for RemoveSession {
            fn name(&self) -> &str {
                "remove_session"
            }
            fn description(&self) -> &str {
                "test write failure"
            }
            fn parameters(&self) -> serde_json::Value {
                json!({})
            }
            fn run<'a>(
                &'a self,
                _: &'a serde_json::Value,
                _: &'a Cancel,
            ) -> BoxFuture<'a, ToolResult> {
                Box::pin(async move {
                    std::fs::remove_file(&self.0).unwrap();
                    ToolResult::ok("removed")
                })
            }
        }
        let dir = crate::testutil::temp_dir("tui-tool-persistence");
        let marker = dir.join("must-not-exist");
        let session = crate::session::Session::create_in(&dir, &dir, "m").unwrap();
        let tool = Arc::new(RemoveSession(session.path.clone()));
        let shared = Shared::new(session, Model::unknown("mock", "m"));
        let provider = Arc::new(ScriptedProvider::new(vec![vec![
            StreamEvent::ToolCallStart {
                index: 0,
                id: "c1".into(),
                name: "remove_session".into(),
            },
            StreamEvent::ToolCallDelta {
                index: 0,
                args: "{}".into(),
            },
            StreamEvent::ToolCallStart {
                index: 1,
                id: "c2".into(),
                name: "write".into(),
            },
            StreamEvent::ToolCallDelta {
                index: 1,
                args: json!({"file_path": marker, "content": "bad"}).to_string(),
            },
            StreamEvent::Stop(StopReason::ToolCalls),
        ]]));
        let agent = Arc::new(Mutex::new(Agent::new(
            provider.clone(),
            shared.model(),
            String::new(),
            vec![tool, Arc::new(crate::tools::WriteTool)],
            vec![],
        )));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let cancel = Cancel::new();
        spawn_turn(
            agent.clone(),
            shared.clone(),
            tx.clone(),
            Submission {
                text: "hello".into(),
                images: vec![],
            },
            cancel.clone(),
        );
        let mut errors = vec![];
        tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(event) = rx.recv().await {
                match event {
                    UiEvent::Error(error) => errors.push(error),
                    UiEvent::Busy(false) => break,
                    UiEvent::Info(message) => assert_ne!(message, "interrupted"),
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("session persistence failed"));
        assert!(cancel.is_cancelled());
        assert!(
            !marker.exists(),
            "remaining tool ran after result persistence failed"
        );
        assert_eq!(provider.observed().len(), 1);
        let before = agent.lock().await.messages().len();

        spawn_turn(
            agent.clone(),
            shared,
            tx,
            Submission {
                text: "again".into(),
                images: vec![],
            },
            Cancel::new(),
        );
        assert!(matches!(rx.recv().await, Some(UiEvent::Error(message)) if message == errors[0]));
        assert!(matches!(rx.recv().await, Some(UiEvent::Busy(false))));
        assert_eq!(agent.lock().await.messages().len(), before);
        assert_eq!(provider.observed().len(), 1);
    }

    #[test]
    fn slash_commands_and_the_help_table_agree() {
        // Mirrors every arm of `slash_command`; update both together.
        let implemented = [
            "/login",
            "/logout",
            "/compact",
            "/model",
            "/remember",
            "/remember global",
            "/forget",
            "/forget global",
            "/memory",
            "/lab",
            "/dream",
            "/help",
        ];
        for name in implemented {
            assert!(
                tui::COMMANDS.iter().any(|command| command.name == name),
                "{name} is implemented but missing from tui::commands"
            );
        }
        for command in tui::COMMANDS {
            assert!(
                !matches!(slash_command(command.name), Some(Slash::Unknown(_))),
                "{} is advertised but unknown to the parser",
                command.name
            );
        }
    }

    #[test]
    fn slash_commands_are_the_first_token() {
        assert_eq!(slash_command("/login"), Some(Slash::Login));
        assert_eq!(slash_command("  /logout  "), Some(Slash::Logout));
        assert_eq!(slash_command("/help me"), Some(Slash::Help));
        assert_eq!(slash_command("/compact"), Some(Slash::Compact));
        assert_eq!(slash_command("/model"), Some(Slash::Model { id: None }));
        assert_eq!(
            slash_command("/model MiniMax-M3.1"),
            Some(Slash::Model {
                id: Some("MiniMax-M3.1".into())
            })
        );
        assert_eq!(slash_command("/memory"), Some(Slash::Memory));
        assert_eq!(slash_command("/lab"), Some(Slash::Lab));
        assert_eq!(slash_command("/lab on"), Some(Slash::Lab));
        assert_eq!(slash_command("/dream"), Some(Slash::Dream));
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

    #[tokio::test]
    async fn model_switch_changes_the_agent_and_the_shared_model() {
        let dir = crate::testutil::temp_dir("model-switch");
        let session = crate::session::Session::create_in(&dir, &dir, "deepseek-flash").unwrap();
        let shared = Shared::new(session, llm::deepseek::model_or_unknown("deepseek-flash"));
        let provider = Arc::new(llm::mock::ScriptedProvider::new(vec![]));
        let agent = Arc::new(Mutex::new(Agent::new(
            provider.clone(),
            shared.model(),
            String::new(),
            vec![],
            vec![],
        )));
        let (tx, mut rx) = mpsc::unbounded_channel();

        switch_model(
            &cli::Args::default(),
            &agent,
            &shared,
            &tx,
            "deepseek-v4-pro",
        )
        .await;

        assert_eq!(agent.lock().await.model().id, "deepseek-v4-pro");
        assert_eq!(shared.model().id, "deepseek-v4-pro");
        // The status bar follows the switch.
        assert!(
            matches!(rx.recv().await, Some(UiEvent::Info(message)) if message.contains("deepseek-v4-pro"))
        );
        let status = match rx.recv().await {
            Some(UiEvent::Status(status)) => status,
            other => panic!("expected a status update, got {other:?}"),
        };
        assert_eq!(status.model, "deepseek-v4-pro");
    }

    #[tokio::test]
    async fn model_switch_to_an_alias_uses_the_id_the_api_accepts() {
        let dir = crate::testutil::temp_dir("model-alias");
        let session = crate::session::Session::create_in(&dir, &dir, "deepseek-flash").unwrap();
        let shared = Shared::new(session, llm::deepseek::model_or_unknown("deepseek-flash"));
        let provider = Arc::new(llm::mock::ScriptedProvider::new(vec![]));
        let agent = Arc::new(Mutex::new(Agent::new(
            provider.clone(),
            shared.model(),
            String::new(),
            vec![],
            vec![],
        )));
        let (tx, mut rx) = mpsc::unbounded_channel();

        // The alias resolves to the M3.1 model of another provider, which
        // needs that provider's credentials.
        switch_model(&cli::Args::default(), &agent, &shared, &tx, "MiniMax-M3.1").await;

        // No key for the other provider, so the switch is refused and the
        // current model stays.
        assert_eq!(agent.lock().await.model().id, "deepseek-flash");
        assert!(
            matches!(rx.recv().await, Some(UiEvent::Error(message)) if message.contains("no API key"))
        );
    }

    #[test]
    fn the_model_list_marks_the_current_model() {
        let dir = crate::testutil::temp_dir("model-list");
        let session = crate::session::Session::create_in(&dir, &dir, "deepseek-flash").unwrap();
        let shared = Shared::new(session, llm::deepseek::model_or_unknown("deepseek-flash"));
        let list = list_models(&cli::Args::default(), &shared);
        assert!(list.contains("* deepseek-flash"), "{list}");
        assert!(list.contains("MiniMax-M3.1-Flash"), "{list}");
        assert!(list.contains("switch with /model <id>"), "{list}");
    }
}
