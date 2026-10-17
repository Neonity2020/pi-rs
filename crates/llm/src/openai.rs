//! OpenAI-compatible chat completions client with SSE streaming.
//!
//! DeepSeek speaks this protocol natively (with two extra knobs, `thinking`
//! and `reasoning_effort`), so one implementation serves both.

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc::UnboundedSender;
use tokio_stream::StreamExt;

use crate::provider::{BoxFuture, ChatRequest, Provider, StreamEvent, Thinking};
use crate::{deepseek, Cancel, Effort, Error, Message, Role, StopReason, Usage};

pub struct OpenAiCompat {
    name: &'static str,
    base_url: String,
    api_key: String,
    /// Present only for DeepSeek-style backends that accept `thinking`.
    thinking: Option<Thinking>,
    effort: Option<Effort>,
    client: reqwest::Client,
}

impl OpenAiCompat {
    pub fn deepseek(
        api_key: impl Into<String>,
        thinking: Thinking,
        effort: Option<Effort>,
    ) -> Self {
        Self {
            name: "deepseek",
            base_url: deepseek::BASE_URL.to_string(),
            api_key: api_key.into(),
            thinking: Some(thinking),
            effort,
            client: default_client(),
        }
    }

    /// Any OpenAI-compatible endpoint (OpenAI, vLLM, Ollama, gateways…).
    pub fn custom(
        name: &'static str,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        Self {
            name,
            base_url: base_url.into(),
            api_key: api_key.into(),
            thinking: None,
            effort: None,
            client: default_client(),
        }
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    fn endpoint(&self) -> String {
        format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
    }
}

fn default_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(concat!("pi-rs/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("failed to build http client")
}

/// Check an API key against an OpenAI-compatible `GET /models` endpoint.
///
/// Login saves a key only after this succeeds, so a typo fails right away
/// instead of on the first prompt. `/models` is free on DeepSeek and OpenAI.
pub async fn verify_api_key(base_url: &str, api_key: &str) -> Result<(), Error> {
    let endpoint = format!("{}/models", base_url.trim_end_matches('/'));
    let response = reqwest::Client::builder()
        .user_agent(concat!("pi-rs/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(15))
        .build()?
        .get(&endpoint)
        .bearer_auth(api_key)
        .send()
        .await?;

    let status = response.status();
    if status.is_success() {
        return Ok(());
    }
    let body = response.text().await.unwrap_or_default();
    Err(Error::Api {
        status: status.as_u16(),
        body: truncate(&body, 2000),
    })
}

impl Provider for OpenAiCompat {
    fn name(&self) -> &'static str {
        self.name
    }

    fn stream<'a>(
        &'a self,
        request: ChatRequest<'a>,
        sink: UnboundedSender<StreamEvent>,
        cancel: Cancel,
    ) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            let body = body_json(self, &request);
            let send = self
                .client
                .post(self.endpoint())
                .bearer_auth(&self.api_key)
                .json(&body)
                .send();

            let response = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(Error::Cancelled),
                r = send => r?,
            };

            let status = response.status();
            if !status.is_success() {
                let body = response.text().await.unwrap_or_default();
                return Err(Error::Api {
                    status: status.as_u16(),
                    body: truncate(&body, 2000),
                });
            }

            let mut stream = response.bytes_stream();
            let mut buf: Vec<u8> = Vec::new();
            loop {
                let chunk = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(Error::Cancelled),
                    c = stream.next() => c,
                };
                let bytes = match chunk {
                    None => break,
                    Some(Ok(bytes)) => bytes,
                    Some(Err(e)) => return Err(Error::Http(e)),
                };
                buf.extend_from_slice(&bytes);
                while let Some(pos) = buf.iter().position(|b| *b == b'\n') {
                    let raw: Vec<u8> = buf.drain(..=pos).collect();
                    let line = String::from_utf8_lossy(&raw[..raw.len() - 1]);
                    match classify_line(line.trim_end_matches('\r')) {
                        Line::Ignore => {}
                        Line::Done => return Ok(()),
                        Line::Data(data) => emit_chunk(data, &sink)?,
                    }
                }
            }
            // Flush a trailing line that had no newline terminator.
            if !buf.is_empty() {
                let line = String::from_utf8_lossy(&buf).into_owned();
                match classify_line(line.trim_end_matches('\r')) {
                    Line::Ignore => {}
                    Line::Done => {}
                    Line::Data(data) => emit_chunk(data, &sink)?,
                }
            }
            Ok(())
        })
    }
}

enum Line<'a> {
    Ignore,
    Done,
    Data(&'a str),
}

fn classify_line(line: &str) -> Line<'_> {
    let Some(rest) = line.strip_prefix("data:") else {
        // `event:` lines, comments (`:`), keep-alives: not used by this protocol.
        return Line::Ignore;
    };
    let data = rest.trim();
    if data == "[DONE]" {
        Line::Done
    } else if data.is_empty() {
        Line::Ignore
    } else {
        Line::Data(data)
    }
}

fn emit_chunk(data: &str, sink: &UnboundedSender<StreamEvent>) -> Result<(), Error> {
    let chunk: WireChunk = serde_json::from_str(data)
        .map_err(|e| Error::Stream(format!("bad chunk ({e}): {}", truncate(data, 500))))?;

    if let Some(err) = chunk.error {
        return Err(Error::Api {
            status: 200,
            body: err.message.unwrap_or_else(|| "stream failed".into()),
        });
    }

    let send =
        |ev: StreamEvent| -> Result<(), Error> { sink.send(ev).map_err(|_| Error::Cancelled) };

    for choice in chunk.choices {
        if let Some(text) = choice.delta.content {
            if !text.is_empty() {
                send(StreamEvent::Text(text))?;
            }
        }
        if let Some(reasoning) = choice.delta.reasoning_content {
            if !reasoning.is_empty() {
                send(StreamEvent::Reasoning(reasoning))?;
            }
        }
        for call in choice.delta.tool_calls {
            let index = call.index.unwrap_or(0);
            if call.id.is_some() || call.function.name.is_some() {
                send(StreamEvent::ToolCallStart {
                    index,
                    id: call.id.unwrap_or_default(),
                    name: call.function.name.unwrap_or_default(),
                })?;
            }
            if let Some(args) = call.function.arguments {
                if !args.is_empty() {
                    send(StreamEvent::ToolCallDelta { index, args })?;
                }
            }
        }
        if let Some(reason) = choice.finish_reason {
            send(StreamEvent::Stop(map_stop(&reason)))?;
        }
    }

    if let Some(usage) = chunk.usage {
        send(StreamEvent::Usage(usage.into()))?;
    }
    Ok(())
}

fn map_stop(reason: &str) -> StopReason {
    match reason {
        "stop" => StopReason::EndTurn,
        "tool_calls" => StopReason::ToolCalls,
        "length" => StopReason::Length,
        "content_filter" => StopReason::ContentFilter,
        _ => StopReason::Other,
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &s[..end])
    }
}

// ---------------------------------------------------------------------------
// Wire format
// ---------------------------------------------------------------------------

fn body_json(provider: &OpenAiCompat, req: &ChatRequest<'_>) -> Value {
    let mut messages = Vec::with_capacity(req.messages.len() + 1);
    if let Some(system) = req.system {
        messages.push(json!({ "role": "system", "content": system }));
    }
    for message in req.messages {
        messages.push(wire_message(message));
    }

    let mut body = json!({
        "model": req.model,
        "messages": messages,
        "stream": true,
        "stream_options": { "include_usage": true },
    });
    let map = body.as_object_mut().expect("object");

    if !req.tools.is_empty() {
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    }
                })
            })
            .collect();
        map.insert("tools".into(), Value::Array(tools));
    }
    if let Some(max) = req.max_tokens {
        map.insert("max_tokens".into(), json!(max));
    }
    if let Some(thinking) = provider.thinking {
        let kind = match thinking {
            Thinking::Enabled => "enabled",
            Thinking::Disabled => "disabled",
        };
        map.insert("thinking".into(), json!({ "type": kind }));
    }
    if let Some(effort) = provider.effort {
        map.insert("reasoning_effort".into(), json!(effort.as_str()));
    }
    body
}

fn wire_message(message: &Message) -> Value {
    let role = match message.role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    };
    let mut value = json!({ "role": role });
    let map = value.as_object_mut().expect("object");

    // DeepSeek: when `tools` is present, reasoning_content of all previous
    // assistant turns must be echoed back verbatim or the API rejects the
    // request. Sending it unconditionally is safe (ignored without tools).
    if let Some(reasoning) = &message.reasoning {
        map.insert("reasoning_content".into(), json!(reasoning));
    }
    if let Some(content) = &message.content {
        map.insert("content".into(), json!(content));
    } else if message.role == Role::Tool {
        map.insert("content".into(), json!(""));
    }
    if !message.tool_calls.is_empty() {
        let calls: Vec<Value> = message
            .tool_calls
            .iter()
            .map(|c| {
                json!({
                    "id": c.id,
                    "type": "function",
                    "function": { "name": c.name, "arguments": c.arguments },
                })
            })
            .collect();
        map.insert("tool_calls".into(), Value::Array(calls));
    }
    if let Some(call_id) = &message.tool_call_id {
        map.insert("tool_call_id".into(), json!(call_id));
    }
    value
}

// ---------------------------------------------------------------------------
// Response chunks
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct WireChunk {
    #[serde(default)]
    choices: Vec<WireChoice>,
    #[serde(default)]
    usage: Option<WireUsage>,
    #[serde(default)]
    error: Option<WireError>,
}

#[derive(Deserialize)]
struct WireChoice {
    #[serde(default)]
    delta: WireDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct WireDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<WireToolCallDelta>,
}

#[derive(Deserialize)]
struct WireToolCallDelta {
    #[serde(default)]
    index: Option<u32>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: WireToolCallFn,
}

#[derive(Deserialize, Default)]
struct WireToolCallFn {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Deserialize)]
struct WireUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    #[serde(default)]
    prompt_cache_hit_tokens: u64,
    #[serde(default)]
    completion_tokens_details: WireCompletionDetails,
}

#[derive(Deserialize, Default)]
struct WireCompletionDetails {
    #[serde(default)]
    reasoning_tokens: u64,
}

#[derive(Deserialize)]
struct WireError {
    #[serde(default)]
    message: Option<String>,
}

impl From<WireUsage> for Usage {
    fn from(w: WireUsage) -> Self {
        Usage {
            input_tokens: w.prompt_tokens,
            output_tokens: w.completion_tokens,
            cached_tokens: w.prompt_cache_hit_tokens,
            reasoning_tokens: w.completion_tokens_details.reasoning_tokens,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::ToolCall;

    fn provider() -> OpenAiCompat {
        OpenAiCompat::deepseek("test-key", Thinking::Enabled, Some(Effort::High))
    }

    fn drain(data: &str) -> Vec<StreamEvent> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        emit_chunk(data, &tx).expect("emit");
        drop(tx);
        let mut events = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            match ev {
                StreamEvent::Text(t) => events.push(StreamEvent::Text(t)),
                StreamEvent::Reasoning(t) => events.push(StreamEvent::Reasoning(t)),
                StreamEvent::ToolCallStart { index, id, name } => {
                    events.push(StreamEvent::ToolCallStart { index, id, name })
                }
                StreamEvent::ToolCallDelta { index, args } => {
                    events.push(StreamEvent::ToolCallDelta { index, args })
                }
                StreamEvent::Usage(u) => events.push(StreamEvent::Usage(u)),
                StreamEvent::Stop(s) => events.push(StreamEvent::Stop(s)),
            }
        }
        events
    }

    #[test]
    fn line_classification() {
        assert!(matches!(classify_line("data: [DONE]"), Line::Done));
        assert!(matches!(classify_line("data: {\"a\":1}"), Line::Data(d) if d == "{\"a\":1}"));
        assert!(matches!(classify_line("data:   {\"a\":1}  "), Line::Data(d) if d == "{\"a\":1}"));
        assert!(matches!(classify_line(": keep-alive"), Line::Ignore));
        assert!(matches!(classify_line("event: message"), Line::Ignore));
        assert!(matches!(classify_line(""), Line::Ignore));
    }

    #[test]
    fn parses_text_and_reasoning_deltas() {
        let events =
            drain(r#"{"choices":[{"delta":{"reasoning_content":"think"},"finish_reason":null}]}"#);
        assert!(matches!(&events[0], StreamEvent::Reasoning(t) if t == "think"));

        let events = drain(r#"{"choices":[{"delta":{"content":"hello"},"finish_reason":null}]}"#);
        assert!(matches!(&events[0], StreamEvent::Text(t) if t == "hello"));

        let events = drain(r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#);
        assert!(matches!(
            &events[0],
            StreamEvent::Stop(StopReason::ToolCalls)
        ));
    }

    #[test]
    fn parses_tool_call_deltas() {
        let events = drain(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"bash","arguments":""}}]}}]}"#,
        );
        assert!(
            matches!(&events[0], StreamEvent::ToolCallStart { index: 0, id, name } if id == "call_1" && name == "bash")
        );

        let events = drain(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"cmd\":"}}]}}]}"#,
        );
        assert!(
            matches!(&events[0], StreamEvent::ToolCallDelta { index: 0, args } if args == "{\"cmd\":")
        );
    }

    #[test]
    fn parses_usage_chunk() {
        let events = drain(
            r#"{"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":20,"prompt_cache_hit_tokens":64,"completion_tokens_details":{"reasoning_tokens":12}}}"#,
        );
        match &events[0] {
            StreamEvent::Usage(u) => {
                assert_eq!(u.input_tokens, 100);
                assert_eq!(u.output_tokens, 20);
                assert_eq!(u.cached_tokens, 64);
                assert_eq!(u.reasoning_tokens, 12);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn stream_error_becomes_api_error() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let err = emit_chunk(r#"{"error":{"message":"rate limited"}}"#, &tx).unwrap_err();
        assert!(matches!(err, Error::Api { body, .. } if body == "rate limited"));
    }

    #[test]
    fn body_serializes_thinking_and_tools() {
        let messages = vec![Message::user("hi")];
        let tools = vec![crate::ToolSpec {
            name: "read".into(),
            description: "Read a file".into(),
            parameters: json!({"type": "object"}),
        }];
        let req = ChatRequest {
            model: "deepseek-flash",
            system: Some("you are pi"),
            messages: &messages,
            tools: &tools,
            max_tokens: None,
        };
        let body = body_json(&provider(), &req);
        assert_eq!(body["model"], "deepseek-flash");
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["reasoning_effort"], "high");
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], "you are pi");
        assert_eq!(body["tools"][0]["function"]["name"], "read");
    }

    #[test]
    fn assistant_reasoning_and_tool_calls_are_echoed_back() {
        let messages = vec![
            Message::user("do it"),
            Message::assistant(
                Some("working".into()),
                Some("because".into()),
                vec![ToolCall {
                    id: "call_1".into(),
                    name: "bash".into(),
                    arguments: "{\"command\":\"ls\"}".into(),
                }],
            ),
            Message::tool("call_1", "file.txt"),
        ];
        let req = ChatRequest {
            model: "m",
            system: None,
            messages: &messages,
            tools: &[],
            max_tokens: None,
        };
        let body = body_json(&provider(), &req);
        let assistant = &body["messages"][1];
        assert_eq!(assistant["reasoning_content"], "because");
        assert_eq!(assistant["content"], "working");
        assert_eq!(assistant["tool_calls"][0]["id"], "call_1");
        assert_eq!(assistant["tool_calls"][0]["function"]["name"], "bash");
        let tool = &body["messages"][2];
        assert_eq!(tool["role"], "tool");
        assert_eq!(tool["tool_call_id"], "call_1");
        assert_eq!(tool["content"], "file.txt");
        assert!(body.get("thinking").is_some());
    }

    #[test]
    fn custom_provider_has_no_thinking_fields() {
        let p = OpenAiCompat::custom("openai", "https://example.com/v1", "k");
        let messages = vec![Message::user("hi")];
        let req = ChatRequest {
            model: "m",
            system: None,
            messages: &messages,
            tools: &[],
            max_tokens: Some(100),
        };
        let body = body_json(&p, &req);
        assert!(body.get("thinking").is_none());
        assert!(body.get("reasoning_effort").is_none());
        assert_eq!(body["max_tokens"], 100);
    }
}
