use std::sync::Arc;
use std::time::Instant;

use tokio::sync::mpsc;
use tokio::task::JoinSet;

use crate::event::{AgentEvent, Sink};
use crate::tool::ToolResult;
use crate::{
    Cancel, ChatRequest, Error, Message, Model, Provider, StreamEvent, Tool, ToolCall, ToolSpec,
};

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
        }
    }

    pub fn model(&self) -> &Model {
        &self.model
    }

    /// Swap the provider without losing history (used after `/login`).
    pub fn set_provider(&mut self, provider: Arc<dyn Provider>) {
        self.provider = provider;
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Run one user turn: append the input, then loop (model request → tool
    /// execution) until the model stops calling tools.
    pub async fn run_turn(
        &mut self,
        input: String,
        sink: &mut dyn Sink,
        cancel: Cancel,
    ) -> Result<Outcome, Error> {
        self.messages.push(Message::user(input));
        let mut step = 0u32;

        loop {
            if cancel.is_cancelled() {
                return Ok(Outcome::Aborted);
            }
            step += 1;
            sink.on(AgentEvent::Step { step });

            let (acc, aborted, failure) = self.stream_step(sink, &cancel).await;
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
            }

            if message.tool_calls.is_empty() {
                return Ok(Outcome::Completed);
            }

            let calls = message.tool_calls.clone();
            for call in &calls {
                sink.on(AgentEvent::ToolStart { call: call.clone() });
            }
            let results = self.execute_tools(&calls, &cancel).await;
            for (call, (result, duration_ms)) in calls.iter().zip(results) {
                sink.on(AgentEvent::ToolEnd {
                    call: call.clone(),
                    result: result.clone(),
                    duration_ms,
                });
                self.messages
                    .push(Message::tool(call.id.clone(), result.content));
            }

            if cancel.is_cancelled() {
                return Ok(Outcome::Aborted);
            }
        }
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

    /// Execute one tool-call batch. Independent calls run in parallel;
    /// results are returned in call order. Every failure mode (unknown tool,
    /// bad JSON, panic, cancellation) becomes an error result.
    async fn execute_tools(&self, calls: &[ToolCall], cancel: &Cancel) -> Vec<(ToolResult, u64)> {
        let mut slots: Vec<Option<(ToolResult, u64)>> = (0..calls.len()).map(|_| None).collect();
        let mut set = JoinSet::new();

        for (i, call) in calls.iter().enumerate() {
            if cancel.is_cancelled() {
                slots[i] = Some((ToolResult::err("Cancelled."), 0));
                continue;
            }
            let Some(tool) = self.tools.iter().find(|t| t.name() == call.name).cloned() else {
                slots[i] = Some((ToolResult::err(format!("No such tool: {}", call.name)), 0));
                continue;
            };
            let args = match parse_args(&call.arguments) {
                Ok(args) => args,
                Err(result) => {
                    slots[i] = Some((result, 0));
                    continue;
                }
            };
            let cancel = cancel.clone();
            set.spawn(async move {
                let started = Instant::now();
                // Inner spawn isolates panics so the outer task always
                // reports its call index.
                let inner = tokio::spawn(async move { tool.run(&args, &cancel).await });
                let result = match inner.await {
                    Ok(result) => result,
                    Err(join_error) => ToolResult::err(format!("tool panicked: {join_error}")),
                };
                (i, result, started.elapsed().as_millis() as u64)
            });
        }

        while let Some(joined) = set.join_next().await {
            if let Ok((i, result, duration_ms)) = joined {
                slots[i] = Some((result, duration_ms));
            }
        }

        slots
            .into_iter()
            .map(|slot| slot.unwrap_or_else(|| (ToolResult::err("Tool did not run."), 0)))
            .collect()
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
            .run_turn("hi".into(), &mut sink, Cancel::new())
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
            .run_turn("run".into(), &mut sink, Cancel::new())
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
            .run_turn("run".into(), &mut sink, Cancel::new())
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
            .run_turn("run".into(), &mut sink, Cancel::new())
            .await
            .unwrap();
        assert!(agent.messages()[2].text().contains("Invalid JSON"));
    }

    #[tokio::test]
    async fn parallel_batch_preserves_call_order() {
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
            .run_turn("run".into(), &mut sink, Cancel::new())
            .await
            .unwrap();
        let messages = agent.messages();
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("c0"));
        assert_eq!(messages[2].text(), "a");
        assert_eq!(messages[3].tool_call_id.as_deref(), Some("c1"));
        assert_eq!(messages[3].text(), "b");
    }

    #[tokio::test]
    async fn abort_before_start() {
        let (mut agent, _p) = agent(vec![]);
        let cancel = Cancel::new();
        cancel.cancel();
        let mut sink = CollectSink::default();
        let outcome = agent.run_turn("x".into(), &mut sink, cancel).await.unwrap();
        assert_eq!(outcome, Outcome::Aborted);
        assert_eq!(agent.messages().len(), 1, "only the user message");
    }

    #[tokio::test]
    async fn empty_stream_is_error() {
        let (mut agent, _p) = agent(vec![vec![]]);
        let mut sink = CollectSink::default();
        let error = agent
            .run_turn("x".into(), &mut sink, Cancel::new())
            .await
            .unwrap_err();
        assert!(matches!(error, Error::Stream(_)));
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
            .run_turn("x".into(), &mut sink, Cancel::new())
            .await
            .unwrap();
        let events = sink.0.lock().unwrap();
        assert!(events.iter().any(|e| matches!(
            e,
            AgentEvent::Usage { usage } if usage.input_tokens == 10
        )));
    }
}
