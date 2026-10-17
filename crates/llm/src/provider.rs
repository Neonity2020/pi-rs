use std::future::Future;
use std::pin::Pin;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;

use crate::{Cancel, Error, Message, Usage};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A tool as the model sees it (OpenAI function-calling format).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolCalls,
    Length,
    ContentFilter,
    Other,
}

/// Thinking-mode toggle (DeepSeek). The default on the wire is "enabled".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Thinking {
    Enabled,
    Disabled,
}

/// Reasoning effort, as accepted by DeepSeek (`low | high | max`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Low,
    High,
    Max,
}

impl Effort {
    pub fn as_str(&self) -> &'static str {
        match self {
            Effort::Low => "low",
            Effort::High => "high",
            Effort::Max => "max",
        }
    }
}

/// Everything needed for one model request. Borrowed for the duration of the
/// stream, so long conversations are not copied.
#[derive(Debug)]
pub struct ChatRequest<'a> {
    pub model: &'a str,
    pub system: Option<&'a str>,
    pub messages: &'a [Message],
    pub tools: &'a [ToolSpec],
    pub max_tokens: Option<u64>,
}

/// Provider-agnostic streaming events. Deltas only — the consumer
/// (agent loop) owns accumulation.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    Text(String),
    Reasoning(String),
    ToolCallStart {
        index: u32,
        id: String,
        name: String,
    },
    ToolCallDelta {
        index: u32,
        args: String,
    },
    Usage(Usage),
    Stop(StopReason),
}

/// The provider boundary. Implement this to add a backend; everything else in
/// the system stays untouched.
pub trait Provider: Send + Sync {
    fn name(&self) -> &'static str;

    /// Stream one model response. Events go through `sink`; the returned
    /// future only carries success/failure. Returning `Err(Cancelled)` (or
    /// dropping the future) aborts the request.
    fn stream<'a>(
        &'a self,
        request: ChatRequest<'a>,
        sink: UnboundedSender<StreamEvent>,
        cancel: Cancel,
    ) -> BoxFuture<'a, Result<(), Error>>;
}
