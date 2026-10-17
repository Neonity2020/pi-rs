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
use ui::crossterm::event::MouseEventKind;

type Tools = Vec<Arc<dyn Tool>>;

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

pub async fn run_print(
    provider: Arc<dyn Provider>,
    model: Model,
    system: String,
    tools: Tools,
    shared: Shared,
    messages: Vec<Message>,
    prompt: String,
) -> anyhow::Result<()> {
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

pub async fn run_json(
    provider: Arc<dyn Provider>,
    model: Model,
    system: String,
    tools: Tools,
    shared: Shared,
    messages: Vec<Message>,
    prompt: String,
) -> anyhow::Result<()> {
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
    provider: Arc<dyn Provider>,
    model: Model,
    system: String,
    tools: Tools,
    shared: Shared,
    messages: Vec<Message>,
    resumed: Option<usize>,
) -> anyhow::Result<()> {
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
                            let cancel = Cancel::new();
                            *cancel_cell.lock().unwrap() = cancel.clone();
                            spawn_turn(agent.clone(), shared.clone(), ui_tx.clone(), text, cancel);
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
