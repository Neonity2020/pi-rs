//! OpenAI-compatible chat completions client with SSE streaming.
//!
//! DeepSeek and MiniMax CN speak this protocol natively (with a few extra
//! knobs, `thinking`, `reasoning_effort`, `reasoning_split`), so one
//! implementation serves all of them.

use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;
use tokio_stream::StreamExt;

use crate::provider::{BoxFuture, ChatRequest, Provider, StreamEvent, Thinking};
use crate::{deepseek, minimax_cn, Cancel, Effort, Error, Message, Role, StopReason, Usage};

const MAX_SSE_LINE_BYTES: usize = 1024 * 1024;
const MAX_ERROR_BYTES: usize = 2000;

pub struct OpenAiCompat {
    name: &'static str,
    base_url: String,
    api_key: String,
    /// Present only for DeepSeek-style backends that accept `thinking`.
    thinking: Option<Thinking>,
    effort: Option<Effort>,
    /// MiniMax: return thinking in `reasoning_content` instead of `<think>`
    /// tags inside `content`.
    reasoning_split: bool,
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
            reasoning_split: false,
            client: default_client(),
        }
    }

    /// MiniMax CN's OpenAI-compatible endpoint. Reasoning is requested as a
    /// separate `reasoning_content` stream so the agent can persist and echo
    /// it back, as required for interleaved tool calls.
    pub fn minimax_cn(api_key: impl Into<String>, effort: Option<Effort>) -> Self {
        Self {
            name: "minimax-cn",
            base_url: minimax_cn::BASE_URL.to_string(),
            api_key: api_key.into(),
            thinking: None,
            effort,
            reasoning_split: true,
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
            reasoning_split: false,
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

fn client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .user_agent(concat!("pi-rs/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(Duration::from_secs(120))
}

fn default_client() -> reqwest::Client {
    client_builder()
        .build()
        .expect("failed to build http client")
}

/// Check an API key against an OpenAI-compatible `GET /models` endpoint.
///
/// Login saves a key only after this succeeds, so a typo fails right away
/// instead of on the first prompt. `/models` is free on DeepSeek and OpenAI.
pub async fn verify_api_key(base_url: &str, api_key: &str) -> Result<(), Error> {
    let endpoint = format!("{}/models", base_url.trim_end_matches('/'));
    let response = client_builder()
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
    let body = error_body(response).await?;
    Err(Error::Api {
        status: status.as_u16(),
        body,
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
                let body = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(Error::Cancelled),
                    body = error_body(response) => body?,
                };
                return Err(Error::Api {
                    status: status.as_u16(),
                    body,
                });
            }

            let mut stream = response.bytes_stream();
            let mut buf: Vec<u8> = Vec::new();
            let mut finish_reason = None;
            let mut saw_tools = false;
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
                // Append one line fragment at a time, never copying an
                // arbitrarily large transport chunk into the line buffer.
                for part in bytes.split_inclusive(|b| *b == b'\n') {
                    if part.len() > MAX_SSE_LINE_BYTES - buf.len() {
                        return Err(Error::Stream("SSE line limit exceeded (1 MiB)".into()));
                    }
                    buf.extend_from_slice(part);
                    if part.last() != Some(&b'\n') {
                        continue;
                    }
                    let line = std::str::from_utf8(&buf[..buf.len() - 1])
                        .map_err(|e| Error::Stream(format!("invalid UTF-8 in SSE: {e}")))?;
                    match classify_line(line.trim_end_matches('\r')) {
                        Line::Ignore => {}
                        Line::Done => return check_completion(finish_reason, saw_tools),
                        Line::Data(data) => {
                            let (reason, tools) = emit_chunk(data, &sink)?;
                            finish_reason = reason.or(finish_reason);
                            saw_tools |= tools;
                        }
                    }
                    buf.clear();
                }
            }
            // Flush a trailing line that had no newline terminator.
            if !buf.is_empty() {
                let line = std::str::from_utf8(&buf)
                    .map_err(|e| Error::Stream(format!("invalid UTF-8 in SSE: {e}")))?;
                match classify_line(line.trim_end_matches('\r')) {
                    Line::Ignore => {}
                    Line::Done => {}
                    Line::Data(data) => {
                        let (reason, tools) = emit_chunk(data, &sink)?;
                        finish_reason = reason.or(finish_reason);
                        saw_tools |= tools;
                    }
                }
            }
            check_completion(finish_reason, saw_tools)
        })
    }
}

/// Error responses are diagnostics, not unbounded downloads. Drop the body
/// as soon as the retained prefix is full, even if the server never closes.
async fn error_body(mut response: reqwest::Response) -> Result<String, Error> {
    let mut bytes = Vec::with_capacity(MAX_ERROR_BYTES);
    while let Some(chunk) = response.chunk().await? {
        let n = chunk.len().min(MAX_ERROR_BYTES - bytes.len());
        bytes.extend_from_slice(&chunk[..n]);
        if bytes.len() == MAX_ERROR_BYTES {
            return Ok(format!(
                "{}… [error response limited to {MAX_ERROR_BYTES} bytes]",
                truncate(&String::from_utf8_lossy(&bytes), MAX_ERROR_BYTES)
            ));
        }
    }
    Ok(truncate(&String::from_utf8_lossy(&bytes), MAX_ERROR_BYTES))
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

/// `finish_reason` confirms the model response finished. Some compatible
/// endpoints omit [DONE], but [DONE] alone cannot certify tool completeness.
fn check_completion(reason: Option<StopReason>, saw_tools: bool) -> Result<(), Error> {
    let Some(reason) = reason else {
        return Err(Error::Stream(
            "stream ended before finish_reason; response is incomplete".into(),
        ));
    };
    if saw_tools && reason != StopReason::ToolCalls {
        return Err(Error::Stream(format!(
            "incomplete tool calls: finish_reason was {reason:?}"
        )));
    }
    Ok(())
}

fn emit_chunk(
    data: &str,
    sink: &UnboundedSender<StreamEvent>,
) -> Result<(Option<StopReason>, bool), Error> {
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

    let mut finish_reason = None;
    let mut saw_tools = false;
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
        saw_tools |= !choice.delta.tool_calls.is_empty();
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
            let reason = map_stop(&reason);
            finish_reason = Some(reason);
            send(StreamEvent::Stop(reason))?;
        }
    }

    if let Some(usage) = chunk.usage {
        send(StreamEvent::Usage(usage.into()))?;
    }
    Ok((finish_reason, saw_tools))
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
    if provider.reasoning_split {
        map.insert("reasoning_split".into(), json!(true));
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
    if !message.images.is_empty() {
        // OpenAI-style multimodal content. Text-only messages keep the plain
        // string form so text-only providers (e.g. DeepSeek) are untouched.
        let mut parts = Vec::with_capacity(message.images.len() + 1);
        if let Some(text) = &message.content {
            if !text.is_empty() {
                parts.push(json!({ "type": "text", "text": text }));
            }
        }
        for image in &message.images {
            parts.push(json!({
                "type": "image_url",
                "image_url": { "url": image.data_url() },
            }));
        }
        map.insert("content".into(), Value::Array(parts));
    } else if let Some(content) = &message.content {
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
    /// DeepSeek's shape.
    #[serde(default)]
    prompt_cache_hit_tokens: u64,
    /// OpenAI/MiniMax shape.
    #[serde(default)]
    prompt_tokens_details: WirePromptDetails,
    #[serde(default)]
    completion_tokens_details: WireCompletionDetails,
}

#[derive(Deserialize, Default)]
struct WirePromptDetails {
    #[serde(default)]
    cached_tokens: u64,
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
            cached_tokens: w
                .prompt_cache_hit_tokens
                .max(w.prompt_tokens_details.cached_tokens),
            reasoning_tokens: w.completion_tokens_details.reasoning_tokens,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::ToolCall;
    use std::time::Duration;

    fn mock_response(
        bytes: Vec<u8>,
        stall: bool,
    ) -> (
        String,
        tokio::sync::oneshot::Receiver<()>,
        std::thread::JoinHandle<()>,
    ) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (ready, received) = tokio::sync::oneshot::channel();
        let task = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            if !drain_request(&mut stream) {
                return;
            }
            let _ = stream.write_all(&bytes);
            let _ = ready.send(());
            if stall {
                let _ = stream.read(&mut [0u8; 1]);
            }
        });
        (url, received, task)
    }

    fn drain_request(stream: &mut std::net::TcpStream) -> bool {
        use std::io::Read;
        let mut request = Vec::new();
        let mut buf = [0; 8192];
        loop {
            let n = stream.read(&mut buf).unwrap();
            if n == 0 {
                return false;
            }
            request.extend_from_slice(&buf[..n]);
            if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                let len: usize = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .unwrap()
                    .trim()
                    .parse()
                    .unwrap();
                if request.len() >= end + 4 + len {
                    return true;
                }
            }
        }
    }

    async fn collect_response(
        p: OpenAiCompat,
        cancel: Cancel,
    ) -> (Result<(), Error>, Vec<StreamEvent>) {
        let messages = [Message::user("test")];
        let request = ChatRequest {
            model: "m",
            system: None,
            messages: &messages,
            tools: &[],
            max_tokens: None,
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let result = p.stream(request, tx, cancel).await;
        let mut events = vec![];
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        (result, events)
    }

    #[tokio::test]
    async fn oversized_sse_lines_fail_before_emitting_their_contents() {
        let body = format!("data: {{\"choices\":[{{\"delta\":{{\"content\":\"partial\"}}}}]}}\n\ndata: {{\"choices\":[{{\"delta\":{{\"content\":\"{}\"}},\"finish_reason\":\"stop\"}}]}}\n\ndata: [DONE]\n\n", "x".repeat(1024 * 1024));
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        let (url, _, server) = mock_response(response.into_bytes(), false);
        let (result, events) =
            collect_response(OpenAiCompat::custom("mock", url, "fake"), Cancel::new()).await;
        assert!(matches!(result, Err(Error::Stream(message)) if message.contains("line limit")));
        let text: String = events
            .into_iter()
            .filter_map(|e| {
                if let StreamEvent::Text(t) = e {
                    Some(t)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(text, "partial");
        tokio::task::spawn_blocking(move || server.join().unwrap())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn error_responses_stop_reading_at_the_byte_budget() {
        let response = format!(
            "HTTP/1.1 500 Error\r\ncontent-length: 10000000\r\nconnection: close\r\n\r\n{}",
            "中".repeat(2000)
        );
        let (url, _, server) = mock_response(response.into_bytes(), true);
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            collect_response(OpenAiCompat::custom("mock", url, "fake"), Cancel::new()),
        )
        .await
        .expect("must not read the entire stalled error body")
        .0;
        assert!(
            matches!(result, Err(Error::Api { status: 500, body }) if body.len() < 2200 && body.contains("limited"))
        );
        tokio::task::spawn_blocking(move || server.join().unwrap())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn cancellation_interrupts_a_stalled_error_body() {
        let (url, ready, server) = mock_response(
            b"HTTP/1.1 500 Error\r\nconnection: close\r\n\r\npartial".to_vec(),
            true,
        );
        let cancel = Cancel::new();
        let task = tokio::spawn(collect_response(
            OpenAiCompat::custom("mock", url, "fake"),
            cancel.clone(),
        ));
        ready.await.unwrap();
        // Let the client consume the headers and block inside the body read,
        // rather than cancelling the already-covered request-header phase.
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel.cancel();
        let result = tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("cancellation must interrupt error body reads")
            .unwrap()
            .0;
        assert!(matches!(result, Err(Error::Cancelled)));
        tokio::task::spawn_blocking(move || server.join().unwrap())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn stalled_response_headers_and_body_time_out() {
        for prefix in ["", "HTTP/1.1 200 OK\r\nconnection: close\r\n\r\ndata: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n"] {
            let (url, _, server) = mock_response(prefix.as_bytes().to_vec(), true);
            let mut p = OpenAiCompat::custom("mock", url, "fake");
            p.client = client_builder().read_timeout(Duration::from_millis(100)).build().unwrap();
            let (result, events) = tokio::time::timeout(Duration::from_secs(1), collect_response(p, Cancel::new())).await.expect("stalled reads must time out");
            assert!(matches!(result, Err(Error::Http(error)) if error.is_timeout()));
            if !prefix.is_empty() { assert!(events.iter().any(|e| matches!(e, StreamEvent::Text(text) if text == "partial"))); }
            tokio::task::spawn_blocking(move || server.join().unwrap()).await.unwrap();
        }
    }

    #[tokio::test]
    async fn a_stalled_tls_handshake_obeys_the_connection_timeout() {
        use std::io::Read;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("https://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut buf = [0; 8192];
            // Drain the ClientHello, then wait without completing TLS.
            while matches!(stream.read(&mut buf), Ok(n) if n > 0) {}
        });
        let mut p = OpenAiCompat::custom("mock", url, "fake");
        p.client = client_builder()
            .connect_timeout(Duration::from_millis(100))
            .read_timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let result =
            tokio::time::timeout(Duration::from_secs(1), collect_response(p, Cancel::new()))
                .await
                .expect("TLS handshake must obey connect timeout")
                .0;
        assert!(matches!(result, Err(Error::Http(error)) if error.is_timeout()));
        tokio::task::spawn_blocking(move || server.join().unwrap())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn active_streams_can_outlive_one_read_timeout() {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            if !drain_request(&mut stream) {
                return;
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nconnection: close\r\n\r\n")
                .unwrap();
            for _ in 0..5 {
                stream
                    .write_all(b"data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n")
                    .unwrap();
                std::thread::sleep(Duration::from_millis(50));
            }
            stream.write_all(b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n").unwrap();
        });
        let mut p = OpenAiCompat::custom("mock", url, "fake");
        p.client = client_builder()
            .read_timeout(Duration::from_millis(200))
            .build()
            .unwrap();
        let (result, events) = collect_response(p, Cancel::new()).await;
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, StreamEvent::Text(_)))
                .count(),
            5
        );
        tokio::task::spawn_blocking(move || server.join().unwrap())
            .await
            .unwrap();
    }

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
    fn parses_openai_style_cached_tokens() {
        let events = drain(
            r#"{"choices":[],"usage":{"prompt_tokens":170,"completion_tokens":303,"prompt_tokens_details":{"cached_tokens":157}}}"#,
        );
        match &events[0] {
            StreamEvent::Usage(u) => {
                assert_eq!(u.input_tokens, 170);
                assert_eq!(u.output_tokens, 303);
                assert_eq!(u.cached_tokens, 157);
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
            system: Some("you are pi-rs"),
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
        assert_eq!(body["messages"][0]["content"], "you are pi-rs");
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
    fn user_images_become_content_parts() {
        let mut message = Message::user("what is this?");
        message.images.push(crate::Image::new("image/png", "AAAA"));

        let wire = wire_message(&message);
        assert_eq!(wire["content"][0]["type"], "text");
        assert_eq!(wire["content"][0]["text"], "what is this?");
        assert_eq!(wire["content"][1]["type"], "image_url");
        assert_eq!(
            wire["content"][1]["image_url"]["url"],
            "data:image/png;base64,AAAA"
        );

        // Image-only: no empty text part.
        let message = Message::user_with_images("", vec![crate::Image::new("image/png", "BBBB")]);
        let wire = wire_message(&message);
        assert_eq!(wire["content"].as_array().unwrap().len(), 1);
        assert_eq!(wire["content"][0]["type"], "image_url");

        // Text-only stays a plain string (DeepSeek compatibility).
        let wire = wire_message(&Message::user("hi"));
        assert_eq!(wire["content"], "hi");
    }

    #[test]
    fn minimax_requests_split_reasoning_without_deepseek_thinking() {
        let p = OpenAiCompat::minimax_cn("test-key", Some(Effort::Max));
        assert_eq!(p.name(), "minimax-cn");
        let messages = vec![Message::user("hi")];
        let req = ChatRequest {
            model: "MiniMax-M3.1-Flash",
            system: None,
            messages: &messages,
            tools: &[],
            max_tokens: None,
        };
        let body = body_json(&p, &req);
        assert_eq!(body["reasoning_split"], true);
        assert_eq!(body["reasoning_effort"], "max");
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn custom_provider_has_no_thinking_or_reasoning_split_fields() {
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
        assert!(body.get("reasoning_split").is_none());
        assert_eq!(body["max_tokens"], 100);
    }
}
