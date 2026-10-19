use std::sync::Arc;
use std::time::Instant;

use tokio::sync::mpsc;
use tokio::task::JoinSet;

use crate::compaction::{
    collect_file_ops, estimate_context_tokens, estimate_messages_tokens, find_cut_index,
    format_file_ops, is_context_overflow, is_summary_message, keep_recent_tokens,
    serialize_conversation, should_compact, summary_char_budget, summary_message, summary_prompt,
    SUMMARIZATION_SYSTEM_PROMPT,
};
use crate::event::{AgentEvent, Sink};
use crate::tool::ToolResult;
use crate::{
    Cancel, ChatRequest, Error, Message, Model, Provider, StopReason, StreamEvent, Tool, ToolCall,
    ToolSpec,
};

/// Why compaction ran. Automatic compaction respects the threshold; manual
/// and overflow modes always try.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionMode {
    Auto,
    Manual,
    Overflow,
}

/// What one compaction changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compaction {
    pub summary: String,
    pub tokens_before: u64,
    pub tokens_after: u64,
    /// Index into the pre-compaction message list where the kept tail starts.
    pub first_kept_index: usize,
}

/// How a turn ended. Failures are `Err`; this only distinguishes "the model
/// finished" from "the user interrupted".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Completed,
    Aborted,
}

pub struct Agent {
    provider: Arc<dyn Provider>,
    model: Model,
    system: String,
    tools: Vec<Arc<dyn Tool>>,
    specs: Vec<ToolSpec>,
    messages: Vec<Message>,
    /// Latest compaction summary, used to update rather than re-summarize.
    previous_summary: Option<String>,
    /// Set after an auto-compaction that could not get below the threshold,
    /// so the loop does not spend a summary call before every step.
    compaction_stalled: bool,
    /// Actual context size from the last assistant usage, plus the number of
    /// messages it covered. More accurate than re-estimating the whole
    /// conversation; cleared whenever compaction rewrites the history.
    last_usage: Option<(u64, usize)>,
}

impl Agent {
    pub fn new(
        provider: Arc<dyn Provider>,
        model: Model,
        system: String,
        tools: Vec<Arc<dyn Tool>>,
        messages: Vec<Message>,
    ) -> Self {
        let specs = tools
            .iter()
            .map(|t| ToolSpec {
                name: t.name().to_string(),
                description: t.description().to_string(),
                parameters: t.parameters(),
            })
            .collect();
        Self {
            provider,
            model,
            system,
            tools,
            specs,
            messages,
            previous_summary: None,
            compaction_stalled: false,
            last_usage: None,
        }
    }

    pub fn model(&self) -> &Model {
        &self.model
    }

    /// The active provider. Used by experimental modes that run an isolated
    /// turn without taking over this agent's history.
    pub fn provider(&self) -> Arc<dyn Provider> {
        Arc::clone(&self.provider)
    }

    /// Swap the provider without losing history (used after `/login`).
    pub fn set_provider(&mut self, provider: Arc<dyn Provider>) {
        self.provider = provider;
    }

    /// Switch to another model without losing history (used by `/model`).
    /// The new model's context window drives compaction from now on.
    pub fn set_model(&mut self, model: Model) {
        self.model = model;
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Continue from a session that already has a compaction summary.
    pub fn set_previous_summary(&mut self, summary: Option<String>) {
        self.previous_summary = summary;
    }

    pub fn previous_summary(&self) -> Option<&str> {
        self.previous_summary.as_deref()
    }

    fn context_tokens(&self) -> u64 {
        match self.last_usage {
            Some((tokens, covered)) if covered > 0 && covered <= self.messages.len() => {
                tokens + estimate_messages_tokens(&self.messages[covered..])
            }
            _ => estimate_context_tokens(&self.system, &self.specs, &self.messages),
        }
    }

    /// Run one user turn: append the input, then loop (model request → tool
    /// execution) until the model stops calling tools.
    pub async fn run_turn(
        &mut self,
        input: Message,
        sink: &mut dyn Sink,
        cancel: Cancel,
    ) -> Result<Outcome, Error> {
        self.messages.push(input);
        self.compaction_stalled = false;
        let mut step = 0u32;
        let mut overflow_retried = false;

        loop {
            if cancel.is_cancelled() {
                return Ok(Outcome::Aborted);
            }

            if let Err(error) = self.maybe_compact(sink, &cancel).await {
                if matches!(error, Error::Cancelled) {
                    return Ok(Outcome::Aborted);
                }
                return Err(error);
            }

            step += 1;
            sink.on(AgentEvent::Step { step });

            let (acc, aborted, failure) = self.stream_step(sink, &cancel).await;
            let usage = acc.usage();
            let empty = acc.was_empty();
            let message = acc.into_message();

            if aborted {
                // Drop tool calls that were cut off mid-stream: their
                // arguments may be incomplete and the provider rejects
                // tool_calls without matching tool results.
                let mut message = message;
                message.tool_calls.clear();
                if message.content.is_some() {
                    sink.on(AgentEvent::Assistant {
                        message: message.clone(),
                    });
                    self.messages.push(message);
                }
                return Ok(Outcome::Aborted);
            }

            if let Some(error) = failure {
                // A context overflow is recoverable once: summarize and retry
                // the same request. Keep any partial text, drop partial tool
                // calls (they have no matching results).
                if is_context_overflow(&error) && !overflow_retried {
                    overflow_retried = true;
                    let mut message = message;
                    message.tool_calls.clear();
                    if message.content.is_some() {
                        sink.on(AgentEvent::Assistant {
                            message: message.clone(),
                        });
                        self.messages.push(message);
                    }
                    match self
                        .compact(CompactionMode::Overflow, sink, cancel.clone())
                        .await
                    {
                        Ok(Some(_)) => continue,
                        Ok(None) => {
                            sink.on(AgentEvent::CompactionFailed {
                                message: "context overflow, but there was nothing left to compact"
                                    .into(),
                            });
                            return Err(error);
                        }
                        Err(Error::Cancelled) => return Ok(Outcome::Aborted),
                        Err(compact_error) => {
                            sink.on(AgentEvent::CompactionFailed {
                                message: format!(
                                    "context overflow recovery failed: {compact_error}"
                                ),
                            });
                            return Err(error);
                        }
                    }
                }

                // Keep whatever text the model managed to produce.
                let mut message = message;
                message.tool_calls.clear();
                if message.content.is_some() {
                    sink.on(AgentEvent::Assistant {
                        message: message.clone(),
                    });
                    self.messages.push(message);
                }
                return Err(error);
            }

            if empty {
                return Err(Error::Stream("provider returned an empty stream".into()));
            }

            if message.content.is_some() || !message.tool_calls.is_empty() {
                sink.on(AgentEvent::Assistant {
                    message: message.clone(),
                });
                self.messages.push(message.clone());
                if let Some(usage) = usage {
                    let total = usage.input_tokens.saturating_add(usage.output_tokens);
                    if total > 0 {
                        self.last_usage = Some((total, self.messages.len()));
                    }
                }
            }

            if message.tool_calls.is_empty() {
                return Ok(Outcome::Completed);
            }

            let calls = message.tool_calls.clone();
            self.execute_tools(&calls, sink, &cancel).await;

            if cancel.is_cancelled() {
                return Ok(Outcome::Aborted);
            }
        }
    }

    /// Compact if the next request would exceed the model window.
    ///
    /// Failures are reported but do not abort the turn: the request may still
    /// fit, and a session the user can keep is better than a dead loop.
    async fn maybe_compact(&mut self, sink: &mut dyn Sink, cancel: &Cancel) -> Result<(), Error> {
        if self.compaction_stalled {
            return Ok(());
        }
        if !should_compact(self.context_tokens(), self.model.context_window) {
            return Ok(());
        }
        match self
            .compact(CompactionMode::Auto, sink, cancel.clone())
            .await
        {
            Ok(Some(_)) => {
                if should_compact(self.context_tokens(), self.model.context_window) {
                    self.compaction_stalled = true;
                    sink.on(AgentEvent::CompactionFailed {
                        message: "compaction did not get below the context threshold".into(),
                    });
                }
                Ok(())
            }
            Ok(None) => {
                self.compaction_stalled = true;
                sink.on(AgentEvent::CompactionFailed {
                    message: "context is over the threshold, but there was nothing to compact"
                        .into(),
                });
                Ok(())
            }
            Err(Error::Cancelled) => Err(Error::Cancelled),
            Err(error) => {
                self.compaction_stalled = true;
                sink.on(AgentEvent::CompactionFailed {
                    message: format!("auto-compaction failed: {error}"),
                });
                Ok(())
            }
        }
    }

    /// Summarize older messages and keep a recent tail.
    ///
    /// The provider request after this starts with a synthetic user message
    /// carrying the summary. Nothing in the session or wire format changes.
    pub async fn compact(
        &mut self,
        mode: CompactionMode,
        sink: &mut dyn Sink,
        cancel: Cancel,
    ) -> Result<Option<Compaction>, Error> {
        let tokens_before = self.context_tokens();
        if mode == CompactionMode::Auto && !should_compact(tokens_before, self.model.context_window)
        {
            return Ok(None);
        }

        // A resumed session starts with the summary message. It is passed as
        // `previous_summary`, not summarized again as conversation.
        let start = usize::from(self.messages.first().is_some_and(is_summary_message));
        let keep = keep_recent_tokens(self.model.context_window);
        let Some(cut) = find_cut_index(&self.messages, start, keep) else {
            return Ok(None);
        };
        if cut <= start || cut >= self.messages.len() {
            return Ok(None);
        }

        let old = &self.messages[start..cut];
        if old.is_empty() {
            return Ok(None);
        }
        let conversation =
            serialize_conversation(old, summary_char_budget(self.model.context_window));
        let file_ops = collect_file_ops(old);
        let split_turn = self
            .messages
            .get(cut)
            .is_some_and(|message| message.role == crate::Role::Assistant);
        let prompt = summary_prompt(&conversation, self.previous_summary.as_deref(), split_turn);

        let summary = self.generate_summary(&prompt, sink, &cancel).await?;
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let summary = format!("{}{}", summary.trim(), format_file_ops(&file_ops));

        let kept = self.messages.split_off(cut);
        self.messages = Vec::with_capacity(kept.len() + 1);
        self.messages.push(summary_message(&summary));
        self.messages.extend(kept);
        self.previous_summary = Some(summary.clone());
        self.last_usage = None;
        let tokens_after = self.context_tokens();

        sink.on(AgentEvent::Compaction {
            summary: summary.clone(),
            tokens_before,
            tokens_after,
            first_kept_index: cut,
        });

        Ok(Some(Compaction {
            summary,
            tokens_before,
            tokens_after,
            first_kept_index: cut,
        }))
    }

    /// Run the summarization request. Text only; tool calls and truncated
    /// summaries are errors, never session checkpoints.
    async fn generate_summary(
        &self,
        prompt: &str,
        sink: &mut dyn Sink,
        cancel: &Cancel,
    ) -> Result<String, Error> {
        let messages = [Message::user(prompt)];
        let max_tokens = if self.model.max_output > 0 {
            self.model.max_output.min(8_192)
        } else {
            4_096
        };
        let request = ChatRequest {
            model: &self.model.id,
            system: Some(SUMMARIZATION_SYSTEM_PROMPT),
            messages: &messages,
            tools: &[],
            max_tokens: Some(max_tokens),
        };
        let (tx, mut rx) = mpsc::unbounded_channel::<StreamEvent>();
        let fut = self.provider.stream(request, tx.clone(), cancel.clone());
        tokio::pin!(fut);
        let stop = cancel.cancelled();
        tokio::pin!(stop);

        let mut text = String::new();
        let mut stop_reason = None;
        let mut saw_tool_call = false;
        let mut aborted = false;
        let mut failure = None;

        loop {
            tokio::select! {
                biased;
                _ = &mut stop => {
                    aborted = true;
                    break;
                }
                result = &mut fut => {
                    match result {
                        Ok(()) => {}
                        Err(Error::Cancelled) => aborted = true,
                        Err(error) => failure = Some(error),
                    }
                    break;
                }
                event = rx.recv() => {
                    if let Some(event) = event {
                        match event {
                            StreamEvent::Text(delta) => text.push_str(&delta),
                            StreamEvent::Usage(usage) => sink.on(AgentEvent::Usage { usage }),
                            StreamEvent::Stop(reason) => stop_reason = Some(reason),
                            StreamEvent::ToolCallStart { .. } | StreamEvent::ToolCallDelta { .. } => {
                                saw_tool_call = true;
                            }
                            StreamEvent::Reasoning(_) => {}
                        }
                    }
                }
            }
        }
        while let Ok(event) = rx.try_recv() {
            match event {
                StreamEvent::Text(delta) => text.push_str(&delta),
                StreamEvent::Usage(usage) => sink.on(AgentEvent::Usage { usage }),
                StreamEvent::Stop(reason) => stop_reason = Some(reason),
                StreamEvent::ToolCallStart { .. } | StreamEvent::ToolCallDelta { .. } => {
                    saw_tool_call = true;
                }
                StreamEvent::Reasoning(_) => {}
            }
        }
        drop(tx);

        if aborted {
            return Err(Error::Cancelled);
        }
        if let Some(error) = failure {
            return Err(error);
        }
        if saw_tool_call || matches!(stop_reason, Some(StopReason::ToolCalls)) {
            return Err(Error::Stream(
                "summarization attempted to call a tool".into(),
            ));
        }
        if matches!(stop_reason, Some(StopReason::Length)) {
            return Err(Error::Stream(
                "summarization hit the token cap; the summary is incomplete".into(),
            ));
        }
        if text.trim().is_empty() {
            return Err(Error::Stream("summarization returned no text".into()));
        }
        Ok(text)
    }

    /// Stream one model response into an accumulator.
    async fn stream_step(
        &self,
        sink: &mut dyn Sink,
        cancel: &Cancel,
    ) -> (Acc, bool, Option<Error>) {
        let request = ChatRequest {
            model: &self.model.id,
            system: Some(&self.system),
            messages: &self.messages,
            tools: &self.specs,
            max_tokens: None,
        };
        let (tx, mut rx) = mpsc::unbounded_channel::<StreamEvent>();
        let fut = self.provider.stream(request, tx.clone(), cancel.clone());
        tokio::pin!(fut);
        let stop = cancel.cancelled();
        tokio::pin!(stop);

        let mut acc = Acc::default();
        let mut aborted = false;
        let mut failure = None;

        loop {
            tokio::select! {
                biased;
                _ = &mut stop => {
                    aborted = true;
                    break;
                }
                result = &mut fut => {
                    match result {
                        Ok(()) => {}
                        Err(Error::Cancelled) => aborted = true,
                        Err(error) => failure = Some(error),
                    }
                    break;
                }
                event = rx.recv() => {
                    if let Some(event) = event {
                        acc.push(event, sink);
                    }
                }
            }
        }
        // The provider future is done (or was dropped on abort); drain events
        // that were queued just before it finished.
        while let Ok(event) = rx.try_recv() {
            acc.push(event, sink);
        }
        drop(tx);

        (acc, aborted, failure)
    }

    /// Execute in model order: tools may depend on earlier filesystem changes.
    /// Report each result before dispatching the next call so a sink can cancel
    /// on persistence failure. Skipped calls still get results for replay.
    async fn execute_tools(&mut self, calls: &[ToolCall], sink: &mut dyn Sink, cancel: &Cancel) {
        for call in calls {
            if !cancel.is_cancelled() {
                sink.on(AgentEvent::ToolStart { call: call.clone() });
            }
            let started = Instant::now();
            let result = self.execute_tool(call, cancel).await;
            sink.on(AgentEvent::ToolEnd {
                call: call.clone(),
                result: result.clone(),
                duration_ms: started.elapsed().as_millis() as u64,
            });
            self.messages
                .push(Message::tool(call.id.clone(), result.content));
        }
    }

    async fn execute_tool(&self, call: &ToolCall, cancel: &Cancel) -> ToolResult {
        if cancel.is_cancelled() {
            return ToolResult::err("Cancelled.");
        }
        let Some(tool) = self.tools.iter().find(|t| t.name() == call.name).cloned() else {
            return ToolResult::err(format!("No such tool: {}", call.name));
        };
        let args = match parse_args(&call.arguments) {
            Ok(args) => args,
            Err(result) => return result,
        };
        let tool_cancel = cancel.clone();
        // A single owned task isolates panics and is aborted on drop. No
        // detached inner task may keep mutating files after the turn ends.
        let mut task = JoinSet::new();
        task.spawn(async move { tool.run(&args, &tool_cancel).await });
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                task.abort_all();
                let _ = task.join_next().await;
                ToolResult::err("Cancelled.")
            }
            joined = task.join_next() => match joined {
                Some(Ok(result)) => result,
                Some(Err(error)) => ToolResult::err(format!("tool panicked: {error}")),
                None => ToolResult::err("Tool did not run."),
            }
        }
    }
}

fn parse_args(raw: &str) -> Result<serde_json::Value, ToolResult> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(serde_json::Value::Object(Default::default()));
    }
    serde_json::from_str(raw)
        .map_err(|e| ToolResult::err(format!("Invalid JSON for tool arguments: {e}\nraw: {raw}")))
}

/// Deltas accumulated into a complete assistant message.
#[derive(Default)]
struct Acc {
    text: String,
    reasoning: String,
    calls: Vec<CallAcc>,
    saw_anything: bool,
    usage: Option<crate::Usage>,
}

#[derive(Default)]
struct CallAcc {
    id: String,
    name: String,
    args: String,
}

impl Acc {
    fn push(&mut self, event: StreamEvent, sink: &mut dyn Sink) {
        self.saw_anything = true;
        match event {
            StreamEvent::Text(delta) => {
                self.text.push_str(&delta);
                sink.on(AgentEvent::Text { delta });
            }
            StreamEvent::Reasoning(delta) => {
                self.reasoning.push_str(&delta);
                sink.on(AgentEvent::Reasoning { delta });
            }
            StreamEvent::ToolCallStart { index, id, name } => {
                let call = self.slot(index);
                if !id.is_empty() {
                    call.id = id;
                }
                if !name.is_empty() {
                    call.name = name;
                }
            }
            StreamEvent::ToolCallDelta { index, args } => {
                self.slot(index).args.push_str(&args);
            }
            StreamEvent::Usage(usage) => {
                self.usage = Some(usage);
                sink.on(AgentEvent::Usage { usage });
            }
            StreamEvent::Stop(_) => {}
        }
    }

    fn slot(&mut self, index: u32) -> &mut CallAcc {
        while self.calls.len() <= index as usize {
            self.calls.push(CallAcc::default());
        }
        &mut self.calls[index as usize]
    }

    fn was_empty(&self) -> bool {
        !self.saw_anything
    }

    fn usage(&self) -> Option<crate::Usage> {
        self.usage
    }

    fn into_message(self) -> Message {
        let content = if self.text.is_empty() {
            None
        } else {
            Some(self.text)
        };
        let reasoning = if self.reasoning.is_empty() {
            None
        } else {
            Some(self.reasoning)
        };
        let tool_calls = self
            .calls
            .into_iter()
            .enumerate()
            .filter(|(_, call)| !call.name.is_empty())
            .map(|(i, call)| ToolCall {
                id: if call.id.is_empty() {
                    format!("call_{i}")
                } else {
                    call.id
                },
                name: call.name,
                arguments: call.args,
            })
            .collect();
        Message::assistant(content, reasoning, tool_calls)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::mock::ScriptedProvider;
    use crate::StopReason;
    use serde_json::json;
    use std::sync::{Arc, Mutex as StdMutex};

    struct EchoTool;

    impl Tool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "Echo text back"
        }
        fn parameters(&self) -> serde_json::Value {
            json!({"type": "object", "properties": {"text": {"type": "string"}}})
        }
        fn run<'a>(
            &'a self,
            args: &'a serde_json::Value,
            _cancel: &'a Cancel,
        ) -> crate::BoxFuture<'a, ToolResult> {
            let text = args
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("<missing>")
                .to_string();
            Box::pin(async move { ToolResult::ok(text) })
        }
    }

    #[derive(Default)]
    struct CollectSink(StdMutex<Vec<AgentEvent>>);

    impl Sink for CollectSink {
        fn on(&mut self, event: AgentEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    fn agent(steps: Vec<Vec<StreamEvent>>) -> (Agent, Arc<ScriptedProvider>) {
        let provider = Arc::new(ScriptedProvider::new(steps));
        let model = crate::Model::unknown("mock", "test-model");
        let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(EchoTool)];
        let agent = Agent::new(provider.clone(), model, "system".into(), tools, vec![]);
        (agent, provider)
    }

    fn tool_call(id: &str, name: &str, args: &str) -> Vec<StreamEvent> {
        vec![
            StreamEvent::ToolCallStart {
                index: 0,
                id: id.into(),
                name: name.into(),
            },
            StreamEvent::ToolCallDelta {
                index: 0,
                args: args.into(),
            },
            StreamEvent::Stop(StopReason::ToolCalls),
        ]
    }

    #[tokio::test]
    async fn text_only_turn() {
        let (mut agent, provider) = agent(vec![vec![
            StreamEvent::Text("hello ".into()),
            StreamEvent::Text("world".into()),
            StreamEvent::Stop(StopReason::EndTurn),
        ]]);
        let mut sink = CollectSink::default();
        let outcome = agent
            .run_turn(Message::user("hi"), &mut sink, Cancel::new())
            .await
            .unwrap();
        assert_eq!(outcome, Outcome::Completed);
        assert_eq!(agent.messages().len(), 2);
        assert_eq!(agent.messages()[1].text(), "hello world");
        assert_eq!(provider.observed().len(), 1);
        assert_eq!(
            provider.observed()[0].len(),
            1,
            "system is passed separately"
        );
    }

    #[tokio::test]
    async fn tool_call_roundtrip() {
        let (mut agent, provider) = agent(vec![
            tool_call("call_1", "echo", r#"{"text":"ping"}"#),
            vec![
                StreamEvent::Text("done".into()),
                StreamEvent::Stop(StopReason::EndTurn),
            ],
        ]);
        let mut sink = CollectSink::default();
        let outcome = agent
            .run_turn(Message::user("run"), &mut sink, Cancel::new())
            .await
            .unwrap();
        assert_eq!(outcome, Outcome::Completed);

        let messages = agent.messages();
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[1].role, crate::Role::Assistant);
        assert_eq!(messages[1].tool_calls[0].name, "echo");
        assert_eq!(messages[2].role, crate::Role::Tool);
        assert_eq!(messages[2].text(), "ping");
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(messages[3].text(), "done");

        let observed = provider.observed();
        assert_eq!(observed.len(), 2);
        assert_eq!(observed[1].len(), 3, "second request carries full history");
    }

    #[tokio::test]
    async fn unknown_tool_becomes_error_result() {
        let (mut agent, _p) = agent(vec![
            tool_call("call_1", "nope", "{}"),
            vec![StreamEvent::Text("ok".into())],
        ]);
        let mut sink = CollectSink::default();
        let outcome = agent
            .run_turn(Message::user("run"), &mut sink, Cancel::new())
            .await
            .unwrap();
        assert_eq!(outcome, Outcome::Completed);
        let messages = agent.messages();
        assert_eq!(messages[2].role, crate::Role::Tool);
        assert!(messages[2].text().contains("No such tool"));
        assert_eq!(
            messages[2].tool_call_id.as_deref(),
            Some("call_1"),
            "the model still gets a result for every call"
        );
    }

    #[tokio::test]
    async fn invalid_json_becomes_error_result() {
        let (mut agent, _p) = agent(vec![
            tool_call("call_1", "echo", "not json{{"),
            vec![StreamEvent::Text("ok".into())],
        ]);
        let mut sink = CollectSink::default();
        agent
            .run_turn(Message::user("run"), &mut sink, Cancel::new())
            .await
            .unwrap();
        assert!(agent.messages()[2].text().contains("Invalid JSON"));
    }

    #[tokio::test]
    async fn batch_results_preserve_call_order() {
        let first = vec![
            StreamEvent::ToolCallStart {
                index: 0,
                id: "c0".into(),
                name: "echo".into(),
            },
            StreamEvent::ToolCallDelta {
                index: 0,
                args: r#"{"text":"a"}"#.into(),
            },
            StreamEvent::ToolCallStart {
                index: 1,
                id: "c1".into(),
                name: "echo".into(),
            },
            StreamEvent::ToolCallDelta {
                index: 1,
                args: r#"{"text":"b"}"#.into(),
            },
            StreamEvent::Stop(StopReason::ToolCalls),
        ];
        let (mut agent, _p) = agent(vec![first, vec![StreamEvent::Text("ok".into())]]);
        let mut sink = CollectSink::default();
        agent
            .run_turn(Message::user("run"), &mut sink, Cancel::new())
            .await
            .unwrap();
        let messages = agent.messages();
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("c0"));
        assert_eq!(messages[2].text(), "a");
        assert_eq!(messages[3].tool_call_id.as_deref(), Some("c1"));
        assert_eq!(messages[3].text(), "b");
    }

    #[tokio::test]
    async fn tools_finish_and_report_before_the_next_call_starts() {
        struct Ordered(Arc<StdMutex<Vec<String>>>);
        impl Tool for Ordered {
            fn name(&self) -> &str {
                "ordered"
            }
            fn description(&self) -> &str {
                "test execution order"
            }
            fn parameters(&self) -> serde_json::Value {
                json!({})
            }
            fn run<'a>(
                &'a self,
                args: &'a serde_json::Value,
                _: &'a Cancel,
            ) -> crate::BoxFuture<'a, ToolResult> {
                Box::pin(async move {
                    let name = args["name"].as_str().unwrap();
                    self.0.lock().unwrap().push(format!("start {name}"));
                    if name == "first" {
                        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                    }
                    self.0.lock().unwrap().push(format!("end {name}"));
                    ToolResult::ok(name)
                })
            }
        }
        struct Reporting(Arc<StdMutex<Vec<String>>>);
        impl Sink for Reporting {
            fn on(&mut self, event: AgentEvent) {
                if let AgentEvent::ToolEnd { call, .. } = event {
                    self.0.lock().unwrap().push(format!("report {}", call.id));
                }
            }
        }
        let log = Arc::new(StdMutex::new(vec![]));
        let mut calls = tool_call("first", "ordered", r#"{"name":"first"}"#);
        calls.pop();
        calls.extend([
            StreamEvent::ToolCallStart {
                index: 1,
                id: "second".into(),
                name: "ordered".into(),
            },
            StreamEvent::ToolCallDelta {
                index: 1,
                args: r#"{"name":"second"}"#.into(),
            },
            StreamEvent::Stop(StopReason::ToolCalls),
        ]);
        let provider = Arc::new(ScriptedProvider::new(vec![
            calls,
            vec![StreamEvent::Text("done".into())],
        ]));
        let mut agent = Agent::new(
            provider,
            Model::unknown("mock", "m"),
            String::new(),
            vec![Arc::new(Ordered(log.clone()))],
            vec![],
        );
        agent
            .run_turn(
                Message::user("test"),
                &mut Reporting(log.clone()),
                Cancel::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            *log.lock().unwrap(),
            [
                "start first",
                "end first",
                "report first",
                "start second",
                "end second",
                "report second"
            ]
        );
    }

    #[tokio::test]
    async fn cancellation_after_a_result_prevents_remaining_tools() {
        struct CancelAfterResult(Cancel);
        impl Sink for CancelAfterResult {
            fn on(&mut self, event: AgentEvent) {
                if matches!(event, AgentEvent::ToolEnd { .. }) {
                    self.0.cancel();
                }
            }
        }
        // An unknown second tool is deliberately distinguishable from a
        // skipped call: cancellation must be checked before dispatch.
        let mut calls = tool_call("first", "echo", r#"{"text":"one"}"#);
        calls.pop();
        calls.extend([
            StreamEvent::ToolCallStart {
                index: 1,
                id: "second".into(),
                name: "unknown".into(),
            },
            StreamEvent::ToolCallDelta {
                index: 1,
                args: "{}".into(),
            },
            StreamEvent::Stop(StopReason::ToolCalls),
        ]);
        let (mut agent, provider) = agent(vec![calls]);
        let cancel = Cancel::new();
        let outcome = agent
            .run_turn(
                Message::user("test"),
                &mut CancelAfterResult(cancel.clone()),
                cancel,
            )
            .await
            .unwrap();
        assert_eq!(outcome, Outcome::Aborted);
        assert_eq!(agent.messages()[2].text(), "one");
        assert_eq!(agent.messages()[3].text(), "Cancelled.");
        assert_eq!(provider.observed().len(), 1);
    }

    #[tokio::test]
    async fn panicking_tool_is_an_error_and_the_next_call_still_runs() {
        struct Panics;
        impl Tool for Panics {
            fn name(&self) -> &str {
                "panics"
            }
            fn description(&self) -> &str {
                "test panic isolation"
            }
            fn parameters(&self) -> serde_json::Value {
                json!({})
            }
            fn run<'a>(
                &'a self,
                _: &'a serde_json::Value,
                _: &'a Cancel,
            ) -> crate::BoxFuture<'a, ToolResult> {
                Box::pin(async {
                    panic!("test panic");
                })
            }
        }
        let mut calls = tool_call("first", "panics", "{}");
        calls.pop();
        calls.extend([
            StreamEvent::ToolCallStart {
                index: 1,
                id: "second".into(),
                name: "echo".into(),
            },
            StreamEvent::ToolCallDelta {
                index: 1,
                args: r#"{"text":"still runs"}"#.into(),
            },
            StreamEvent::Stop(StopReason::ToolCalls),
        ]);
        let (mut agent, _) = agent(vec![calls, vec![StreamEvent::Text("done".into())]]);
        agent.tools.push(Arc::new(Panics));
        agent
            .run_turn(Message::user("test"), &mut (), Cancel::new())
            .await
            .unwrap();
        assert!(agent.messages()[2].text().contains("tool panicked"));
        assert_eq!(agent.messages()[3].text(), "still runs");
    }

    #[tokio::test]
    async fn cancelling_an_uncooperative_tool_drops_it_before_returning() {
        use std::sync::atomic::{AtomicBool, Ordering};
        struct Flag(Arc<AtomicBool>);
        impl Drop for Flag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        struct Waiting {
            started: Arc<tokio::sync::Notify>,
            dropped: Arc<AtomicBool>,
        }
        impl Tool for Waiting {
            fn name(&self) -> &str {
                "waiting"
            }
            fn description(&self) -> &str {
                "ignores cancellation"
            }
            fn parameters(&self) -> serde_json::Value {
                json!({})
            }
            fn run<'a>(
                &'a self,
                _: &'a serde_json::Value,
                _: &'a Cancel,
            ) -> crate::BoxFuture<'a, ToolResult> {
                Box::pin(async move {
                    let _guard = Flag(self.dropped.clone());
                    self.started.notify_one();
                    std::future::pending().await
                })
            }
        }
        let started = Arc::new(tokio::sync::Notify::new());
        let dropped = Arc::new(AtomicBool::new(false));
        let (mut agent, _) = agent(vec![tool_call("c1", "waiting", "{}")]);
        agent.tools.push(Arc::new(Waiting {
            started: started.clone(),
            dropped: dropped.clone(),
        }));
        let cancel = Cancel::new();
        let handle = tokio::spawn({
            let cancel = cancel.clone();
            async move { agent.run_turn(Message::user("test"), &mut (), cancel).await }
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), started.notified())
            .await
            .unwrap();
        cancel.cancel();
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(outcome, Outcome::Aborted);
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn abort_before_start() {
        let (mut agent, _p) = agent(vec![]);
        let cancel = Cancel::new();
        cancel.cancel();
        let mut sink = CollectSink::default();
        let outcome = agent
            .run_turn(Message::user("x"), &mut sink, cancel)
            .await
            .unwrap();
        assert_eq!(outcome, Outcome::Aborted);
        assert_eq!(agent.messages().len(), 1, "only the user message");
    }

    #[tokio::test]
    async fn empty_stream_is_error() {
        let (mut agent, _p) = agent(vec![vec![]]);
        let mut sink = CollectSink::default();
        let error = agent
            .run_turn(Message::user("x"), &mut sink, Cancel::new())
            .await
            .unwrap_err();
        assert!(matches!(error, Error::Stream(_)));
    }

    #[tokio::test]
    async fn user_images_reach_the_provider() {
        let (mut agent, provider) = agent(vec![vec![StreamEvent::Text("seen".into())]]);
        let message =
            Message::user_with_images("look", vec![crate::Image::new("image/png", "AAAA")]);
        let mut sink = CollectSink::default();
        agent
            .run_turn(message, &mut sink, Cancel::new())
            .await
            .unwrap();
        let observed = provider.observed();
        assert_eq!(observed[0][0].images.len(), 1);
        assert_eq!(observed[0][0].images[0].data, "AAAA");
    }

    fn long_history(pairs: usize) -> Vec<Message> {
        let text = "x".repeat(40);
        let mut messages = Vec::new();
        for i in 0..pairs {
            messages.push(Message::user(format!("{i} {text}")));
            messages.push(Message::assistant(
                Some(format!("{i} {text}")),
                None,
                vec![],
            ));
        }
        messages
    }

    fn small_window_agent(
        steps: Vec<Vec<StreamEvent>>,
        history: Vec<Message>,
    ) -> (Agent, Arc<ScriptedProvider>) {
        let provider = Arc::new(ScriptedProvider::new(steps));
        let mut model = crate::Model::unknown("mock", "small");
        model.context_window = 200;
        let agent = Agent::new(provider.clone(), model, "system".into(), vec![], history);
        (agent, provider)
    }

    #[tokio::test]
    async fn auto_compaction_summarizes_before_the_request() {
        let (mut agent, provider) = small_window_agent(
            vec![
                vec![
                    StreamEvent::Text("## Goal\nkeep going".into()),
                    StreamEvent::Stop(StopReason::EndTurn),
                ],
                vec![
                    StreamEvent::Text("done".into()),
                    StreamEvent::Stop(StopReason::EndTurn),
                ],
            ],
            long_history(10),
        );
        let mut sink = CollectSink::default();
        let outcome = agent
            .run_turn(Message::user("latest"), &mut sink, Cancel::new())
            .await
            .unwrap();
        assert_eq!(outcome, Outcome::Completed);
        assert_eq!(agent.messages().first().unwrap().role, crate::Role::User);
        assert!(crate::is_summary_message(agent.messages().first().unwrap()));
        assert!(agent
            .messages()
            .first()
            .unwrap()
            .text()
            .contains("keep going"));

        let observed = provider.observed();
        assert_eq!(observed.len(), 2, "summary call, then the model call");
        assert!(crate::is_summary_message(&observed[1][0]));
        assert!(
            observed[1].len() < 21,
            "the request should be smaller than the history"
        );
        assert!(matches!(
            sink.0.lock().unwrap().iter().find(|event| matches!(event, AgentEvent::Compaction { .. })),
            Some(AgentEvent::Compaction { first_kept_index, .. }) if *first_kept_index > 0
        ));
    }

    #[tokio::test]
    async fn a_previous_summary_is_updated_not_resummarized() {
        let mut history = vec![crate::summary_message("old summary")];
        history.extend(long_history(10));
        let (mut agent, provider) = small_window_agent(
            vec![
                vec![
                    StreamEvent::Text("updated summary".into()),
                    StreamEvent::Stop(StopReason::EndTurn),
                ],
                vec![
                    StreamEvent::Text("ok".into()),
                    StreamEvent::Stop(StopReason::EndTurn),
                ],
            ],
            history,
        );
        agent.set_previous_summary(Some("old summary".into()));
        let mut sink = CollectSink::default();
        agent
            .run_turn(Message::user("latest"), &mut sink, Cancel::new())
            .await
            .unwrap();

        let observed = provider.observed();
        let prompt = observed[0][0].text();
        assert!(prompt.contains("<previous-summary>\nold summary\n</previous-summary>"));
        assert!(!prompt.contains("<summary>\nold summary"));
        assert!(crate::is_summary_message(&observed[1][0]));
        assert!(observed[1][0].text().contains("updated summary"));
    }

    #[tokio::test]
    async fn manual_compaction_works_without_a_known_window() {
        let (mut agent, _provider) = agent(vec![vec![
            StreamEvent::Text("manual summary".into()),
            StreamEvent::Stop(StopReason::EndTurn),
        ]]);
        // Default test model has context_window 0, but manual compaction can
        // still use the default keep budget.
        let long = "x".repeat(100_000);
        agent.messages = vec![
            Message::user(format!("old {long}")),
            Message::assistant(Some("old answer".into()), None, vec![]),
            Message::user("latest"),
        ];
        let mut sink = CollectSink::default();
        let result = agent
            .compact(CompactionMode::Manual, &mut sink, Cancel::new())
            .await
            .unwrap();
        assert!(result.is_some());
        assert!(crate::is_summary_message(agent.messages().first().unwrap()));
        assert_eq!(agent.messages().last().unwrap().text(), "latest");
    }

    #[tokio::test]
    async fn manual_compaction_reports_nothing_to_do() {
        let (mut agent, _provider) = agent(vec![]);
        let mut sink = CollectSink::default();
        let result = agent
            .compact(CompactionMode::Manual, &mut sink, Cancel::new())
            .await
            .unwrap();
        assert!(result.is_none());
    }

    struct OverflowOnce {
        calls: StdMutex<usize>,
        observed: StdMutex<Vec<Vec<Message>>>,
    }

    impl Default for OverflowOnce {
        fn default() -> Self {
            Self {
                calls: StdMutex::new(0),
                observed: StdMutex::new(Vec::new()),
            }
        }
    }

    impl crate::Provider for OverflowOnce {
        fn name(&self) -> &'static str {
            "overflow-once"
        }

        fn stream<'a>(
            &'a self,
            request: ChatRequest<'a>,
            sink: mpsc::UnboundedSender<StreamEvent>,
            _cancel: Cancel,
        ) -> crate::BoxFuture<'a, Result<(), Error>> {
            self.observed
                .lock()
                .unwrap()
                .push(request.messages.to_vec());
            let call = {
                let mut calls = self.calls.lock().unwrap();
                let call = *calls;
                *calls += 1;
                call
            };
            Box::pin(async move {
                match call {
                    0 => Err(Error::Api {
                        status: 400,
                        body: "This model's maximum context length is 100 tokens".into(),
                    }),
                    1 => {
                        let _ = sink.send(StreamEvent::Text("## Goal\nrecovered".into()));
                        let _ = sink.send(StreamEvent::Stop(StopReason::EndTurn));
                        Ok(())
                    }
                    _ => {
                        let _ = sink.send(StreamEvent::Text("done".into()));
                        let _ = sink.send(StreamEvent::Stop(StopReason::EndTurn));
                        Ok(())
                    }
                }
            })
        }
    }

    #[tokio::test]
    async fn context_overflow_compacts_and_retries_once() {
        let provider = Arc::new(OverflowOnce::default());
        let mut agent = Agent::new(
            provider.clone(),
            crate::Model::unknown("mock", "overflow"),
            "system".into(),
            vec![],
            vec![
                Message::user(format!("old {}", "x".repeat(100_000))),
                Message::assistant(Some("old answer".into()), None, vec![]),
            ],
        );
        let mut sink = CollectSink::default();
        let outcome = agent
            .run_turn(Message::user("latest"), &mut sink, Cancel::new())
            .await
            .unwrap();
        assert_eq!(outcome, Outcome::Completed);
        let observed = provider.observed.lock().unwrap();
        assert_eq!(observed.len(), 3, "overflow, summary, retry");
        assert!(crate::is_summary_message(&observed[2][0]));
        assert_eq!(agent.messages().last().unwrap().text(), "done");
    }

    #[test]
    fn usage_backed_context_estimate_only_counts_trailing_messages() {
        let (mut agent, _provider) = agent(vec![]);
        agent.messages = vec![
            Message::user("x".repeat(4_000)),
            Message::assistant(Some("y".repeat(4_000)), None, vec![]),
        ];
        let without_usage = agent.context_tokens();
        agent.last_usage = Some((10, 2));
        assert!(agent.context_tokens() < without_usage);

        agent.messages.push(Message::user("trailing"));
        assert_eq!(
            agent.context_tokens(),
            10 + crate::estimate_tokens(&agent.messages[2])
        );
    }

    #[tokio::test]
    async fn usage_events_reach_the_sink() {
        let (mut agent, _p) = agent(vec![vec![
            StreamEvent::Text("hi".into()),
            StreamEvent::Usage(crate::Usage {
                input_tokens: 10,
                output_tokens: 2,
                cached_tokens: 4,
                reasoning_tokens: 0,
            }),
        ]]);
        let mut sink = CollectSink::default();
        agent
            .run_turn(Message::user("x"), &mut sink, Cancel::new())
            .await
            .unwrap();
        let events = sink.0.lock().unwrap();
        assert!(events.iter().any(|e| matches!(
            e,
            AgentEvent::Usage { usage } if usage.input_tokens == 10
        )));
    }
}
