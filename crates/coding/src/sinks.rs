//! Sinks bridge the agent event stream to the outside world: terminal
//! output, JSONL, or the TUI. Every sink persists messages and usage into
//! the session file and stats along the way.

use std::io::{IsTerminal, Write};
use std::sync::{Arc, Mutex};

use agent::{AgentEvent, Sink};
use llm::{Message, Model};

use crate::session::Session;
use crate::stats::Stats;
use crate::util;

/// State shared by all sinks and the interactive loop.
#[derive(Clone)]
pub struct Shared {
    pub session: Arc<Mutex<Session>>,
    pub stats: Arc<Mutex<Stats>>,
    pub model: Model,
}

impl Shared {
    pub fn new(session: Session, model: Model) -> Self {
        Self {
            session: Arc::new(Mutex::new(session)),
            stats: Arc::new(Mutex::new(Stats::default())),
            model,
        }
    }

    /// Persist whatever this event contributes to the session.
    pub fn record(&self, event: &AgentEvent) {
        match event {
            AgentEvent::Assistant { message } => {
                let _ = self.session.lock().unwrap().append_message(message);
            }
            AgentEvent::ToolEnd { call, result, .. } => {
                let _ = self
                    .session
                    .lock()
                    .unwrap()
                    .append_message(&Message::tool(call.id.clone(), result.content.clone()));
            }
            AgentEvent::Usage { usage } => {
                self.stats.lock().unwrap().add(*usage, &self.model);
            }
            _ => {}
        }
    }

    pub fn status_info(&self) -> crate::ui::StatusInfo {
        let stats = self.stats.lock().unwrap();
        let session = self.session.lock().unwrap();
        crate::ui::StatusInfo {
            model: self.model.id.clone(),
            dir: std::env::current_dir()
                .ok()
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                .unwrap_or_default(),
            session: session
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            input_tokens: stats.usage.input_tokens,
            output_tokens: stats.usage.output_tokens,
            cached_tokens: stats.usage.cached_tokens,
            cost: stats.cost_display(&self.model),
        }
    }
}

fn dim() -> (String, String) {
    if std::io::stderr().is_terminal() {
        ("\x1b[2m".into(), "\x1b[0m".into())
    } else {
        (String::new(), String::new())
    }
}

/// Print mode: assistant text to stdout, tool activity to stderr.
pub struct PrintSink {
    pub shared: Shared,
    pub wrote_text: bool,
}

impl PrintSink {
    pub fn new(shared: Shared) -> Self {
        Self {
            shared,
            wrote_text: false,
        }
    }
}

impl Sink for PrintSink {
    fn on(&mut self, event: AgentEvent) {
        self.shared.record(&event);
        let (d, r) = dim();
        match event {
            AgentEvent::Text { delta } => {
                print!("{delta}");
                let _ = std::io::stdout().flush();
                self.wrote_text = true;
            }
            AgentEvent::ToolStart { call } => {
                eprintln!(
                    "{d}⏺ {}({}){r}",
                    call.name,
                    util::summarize_args(&call.arguments)
                );
            }
            AgentEvent::ToolEnd {
                result,
                duration_ms,
                ..
            } => {
                let mark = if result.is_error { "✗" } else { "✓" };
                eprintln!("{d}  {mark} {duration_ms}ms{r}");
                if result.is_error {
                    for line in result.content.lines().take(3) {
                        eprintln!("{d}  {line}{r}");
                    }
                }
            }
            _ => {}
        }
    }
}

/// JSON mode: every agent event as one JSONL line on stdout.
pub struct JsonSink {
    pub shared: Shared,
}

impl Sink for JsonSink {
    fn on(&mut self, event: AgentEvent) {
        self.shared.record(&event);
        match serde_json::to_string(&event) {
            Ok(line) => println!("{line}"),
            Err(error) => eprintln!("failed to serialize event: {error}"),
        }
    }
}
