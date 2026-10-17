//! The one place where the TUI crate is wired into the app layer.

pub use tui::render as tui_render;
pub use tui::{crossterm, ratatui};
pub use tui::{Action, App, ImageAttachment, StatusInfo, Submission, UiEvent};

use agent::{AgentEvent, Sink};

use crate::sinks::Shared;
use crate::util;

/// Translate agent events into UI events, persisting on the way.
pub struct TuiSink {
    pub shared: Shared,
    pub ui: tokio::sync::mpsc::UnboundedSender<UiEvent>,
}

impl Sink for TuiSink {
    fn on(&mut self, event: AgentEvent) {
        self.shared.record(&event);
        let translated = match event {
            AgentEvent::Step { .. } => None,
            AgentEvent::Text { delta } => Some(UiEvent::AssistantDelta(delta)),
            AgentEvent::Reasoning { delta } => Some(UiEvent::ReasoningDelta(delta)),
            AgentEvent::Assistant { message } => Some(UiEvent::AssistantDone {
                text: message.content.unwrap_or_default(),
            }),
            AgentEvent::ToolStart { call } => Some(UiEvent::ToolStart {
                id: call.id,
                name: call.name,
                summary: util::summarize_args(&call.arguments),
            }),
            AgentEvent::ToolEnd {
                call,
                result,
                duration_ms,
            } => Some(UiEvent::ToolDone {
                id: call.id,
                ok: !result.is_error,
                output: result.content,
                duration_ms,
            }),
            AgentEvent::Compaction {
                tokens_before,
                tokens_after,
                ..
            } => Some(UiEvent::Info(format!(
                "context compacted: {} → {} tokens",
                crate::stats::format_tokens(tokens_before),
                crate::stats::format_tokens(tokens_after)
            ))),
            AgentEvent::CompactionFailed { message } => Some(UiEvent::Error(message)),
            AgentEvent::Usage { .. } => Some(UiEvent::Status(self.shared.status_info())),
        };
        if let Some(event) = translated {
            let _ = self.ui.send(event);
        }
    }
}
