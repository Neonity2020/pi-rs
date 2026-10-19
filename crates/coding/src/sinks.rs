//! Sinks bridge the agent event stream to the outside world: terminal
//! output, JSONL, or the TUI. Every sink persists messages and usage into
//! the session file and stats along the way.

use std::io::{IsTerminal, Write};
use std::sync::{Arc, Mutex};

use agent::{AgentEvent, Cancel, Sink};
use anyhow::{bail, Result};
use llm::{Message, Model};

use crate::session::Session;
use crate::stats::Stats;
use crate::util;

/// State shared by all sinks and the interactive loop.
#[derive(Clone)]
pub struct Shared {
    pub session: Arc<Mutex<Session>>,
    pub stats: Arc<Mutex<Stats>>,
    /// The active model. Shared behind a lock because `/model` can switch it
    /// while a turn is running.
    model: Arc<Mutex<Model>>,
    persistence_error: Arc<Mutex<Option<String>>>,
    active_cancel: Arc<Mutex<Cancel>>,
}

impl Shared {
    pub fn new(session: Session, model: Model) -> Self {
        Self {
            session: Arc::new(Mutex::new(session)),
            stats: Arc::new(Mutex::new(Stats::default())),
            model: Arc::new(Mutex::new(model)),
            persistence_error: Arc::new(Mutex::new(None)),
            active_cancel: Arc::new(Mutex::new(Cancel::new())),
        }
    }

    /// The active model.
    pub fn model(&self) -> Model {
        self.model.lock().unwrap().clone()
    }

    /// Switch models (`/model`). Cost estimates from now on use the new
    /// model's pricing; earlier usage keeps the price it was billed at.
    pub fn set_model(&self, model: Model) {
        *self.model.lock().unwrap() = model;
    }

    /// A failed write can leave a partial record, so never retry writes or
    /// run another turn with this process's now-divergent in-memory history.
    pub fn persistence_result(&self) -> Result<()> {
        if let Some(error) = &*self.persistence_error.lock().unwrap() {
            bail!("{error}");
        }
        Ok(())
    }

    pub fn begin_turn(&self, cancel: Cancel) -> Result<()> {
        self.persistence_result()?;
        *self.active_cancel.lock().unwrap() = cancel;
        Ok(())
    }

    fn persist(&self, write: impl FnOnce(&mut Session) -> Result<()>) -> Result<()> {
        let mut failure = self.persistence_error.lock().unwrap();
        if let Some(error) = &*failure {
            bail!("{error}");
        }
        let mut session = self.session.lock().unwrap();
        if let Err(error) = write(&mut session) {
            let message = format!(
                "session persistence failed ({}): {error:#}; turn stopped. Repair the session and restart before continuing",
                session.path.display()
            );
            *failure = Some(message.clone());
            // Sink::on is synchronous; cancel before returning to the agent
            // so it cannot start tools from an unpersisted assistant message.
            self.active_cancel.lock().unwrap().cancel();
            bail!("{message}");
        }
        Ok(())
    }

    pub fn append_message(&self, message: &Message) -> Result<()> {
        self.persist(|session| session.append_message(message))
    }

    /// Persist whatever this event contributes to the session.
    pub fn record(&self, event: &AgentEvent) -> Result<()> {
        match event {
            AgentEvent::Assistant { message } => self.append_message(message),
            AgentEvent::ToolEnd { call, result, .. } => {
                self.append_message(&Message::tool(call.id.clone(), result.content.clone()))
            }
            AgentEvent::Compaction {
                summary,
                tokens_before,
                tokens_after,
                first_kept_index,
            } => self.persist(|session| {
                session.append_compaction(summary, *first_kept_index, *tokens_before, *tokens_after)
            }),
            AgentEvent::Usage { usage } => {
                let model = self.model();
                self.stats.lock().unwrap().add(*usage, &model);
                Ok(())
            }
            _ => self.persistence_result(),
        }
    }

    pub fn status_info(&self) -> crate::ui::StatusInfo {
        let stats = self.stats.lock().unwrap();
        let session = self.session.lock().unwrap();
        let model = self.model();
        crate::ui::StatusInfo {
            model: model.id.clone(),
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
            cost: stats.cost_display(&model),
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
        if self.shared.record(&event).is_err() {
            return;
        }
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
            AgentEvent::Compaction {
                tokens_before,
                tokens_after,
                ..
            } => {
                eprintln!(
                    "{d}⏺ compacted context: {} → {} tokens{r}",
                    crate::stats::format_tokens(tokens_before),
                    crate::stats::format_tokens(tokens_after)
                );
            }
            AgentEvent::CompactionFailed { message } => {
                eprintln!("{d}warning: {message}{r}");
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
        if self.shared.record(&event).is_err() {
            return;
        }
        match serde_json::to_string(&event) {
            Ok(line) => println!("{line}"),
            Err(error) => eprintln!("failed to serialize event: {error}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent::{AgentEvent, Message};
    use std::path::Path;

    #[test]
    fn compaction_events_extend_the_session_branch() {
        let dir = crate::testutil::temp_dir("sink-compaction");
        let mut session = Session::create_in(&dir, Path::new("/tmp/project"), "m").unwrap();
        session.append_message(&Message::user("old")).unwrap();
        session
            .append_message(&Message::assistant(Some("old answer".into()), None, vec![]))
            .unwrap();
        let shared = Shared::new(session, Model::unknown("mock", "m"));

        shared
            .record(&AgentEvent::Compaction {
                summary: "summary".into(),
                tokens_before: 100,
                tokens_after: 20,
                first_kept_index: 1,
            })
            .unwrap();
        shared
            .record(&AgentEvent::Assistant {
                message: Message::assistant(Some("new".into()), None, vec![]),
            })
            .unwrap();

        let (loaded, messages) = Session::load_latest_in(&dir).unwrap().unwrap();
        assert_eq!(loaded.summary(), Some("summary"));
        assert_eq!(messages.len(), 3);
        assert!(agent::is_summary_message(&messages[0]));
        assert_eq!(messages[1].text(), "old answer");
        assert_eq!(messages[2].text(), "new");
    }

    #[test]
    fn every_record_failure_cancels_and_latches_until_restart() {
        let events = [
            AgentEvent::Assistant {
                message: Message::assistant(Some("answer".into()), None, vec![]),
            },
            AgentEvent::ToolEnd {
                call: llm::ToolCall {
                    id: "c1".into(),
                    name: "read".into(),
                    arguments: "{}".into(),
                },
                result: agent::ToolResult::ok("result"),
                duration_ms: 0,
            },
            AgentEvent::Compaction {
                summary: "summary".into(),
                first_kept_index: 0,
                tokens_before: 10,
                tokens_after: 5,
            },
        ];
        for event in events {
            let dir = crate::testutil::temp_dir("sink-failure");
            let mut session = Session::create_in(&dir, Path::new("/tmp/project"), "m").unwrap();
            session.append_message(&Message::user("one")).unwrap();
            let path = session.path.clone();
            let backup = path.with_extension("backup");
            let shared = Shared::new(session, Model::unknown("mock", "m"));
            let cancel = Cancel::new();
            shared.begin_turn(cancel.clone()).unwrap();
            std::fs::rename(&path, &backup).unwrap();
            std::fs::create_dir(&path).unwrap();

            let error = shared.record(&event).unwrap_err().to_string();
            assert!(error.contains("session persistence failed"));
            assert!(cancel.is_cancelled());
            std::fs::remove_dir(&path).unwrap();
            std::fs::rename(&backup, &path).unwrap();
            let original = std::fs::read(&path).unwrap();
            assert_eq!(
                shared.begin_turn(Cancel::new()).unwrap_err().to_string(),
                error
            );
            assert_eq!(
                shared
                    .append_message(&Message::user("two"))
                    .unwrap_err()
                    .to_string(),
                error
            );
            assert_eq!(std::fs::read(&path).unwrap(), original);
        }
    }
}
