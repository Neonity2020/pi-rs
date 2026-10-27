//! OpenAI-compatible Chat Completions adapter.
//! Responses are currently collected before emitting events.
use crate::model::Model;
use crate::stream::{
	AssistantMessageEvent as Event, AssistantMessageEventStream, StreamFn, StreamOptions,
	TranscriptContext,
};
use crate::types::{
	AssistantContent, AssistantMessage, Message, StopReason, SystemContent, TextContent,
	TextOrImage, ToolCall, UserContent,
};
use serde_json::{Value, json};
use std::sync::Arc;

pub fn stream_fn() -> StreamFn {
	Arc::new(
		|model: &Model,
		 context: &TranscriptContext,
		 options: StreamOptions|
		 -> AssistantMessageEventStream {
			let model = model.clone();
			let context = context.clone();
			Box::pin(
				futures::stream::once(async move {
					let result = tokio::select! {
						_ = options.signal.cancelled() => Err((StopReason::Aborted, "aborted by user".to_string())),
						result = request(&model, &context, &options) => result.map_err(|e| (StopReason::Error, e)),
					};
					match result {
						Ok(events) => events,
						Err((reason, message)) => vec![Event::Error {
							reason,
							error: AssistantMessage::error(&model, reason, message),
						}],
					}
				})
				.flat_map(futures::stream::iter),
			)
		},
	)
}
use futures::StreamExt;

fn request_body(model: &Model, context: &TranscriptContext, options: &StreamOptions) -> Value {
	let mut messages = Vec::new();
	let mut tools = Vec::new();
	for message in &context.messages {
		match message {
			Message::System(m) => {
				let text = match &m.content {
					SystemContent::Text(s) => s.clone(),
					SystemContent::Blocks(b) => b
						.iter()
						.map(|x| x.text.as_str())
						.collect::<Vec<_>>()
						.join("\n"),
				};
				if !text.is_empty() {
					messages.push(json!({"role":"system","content":text}));
				}
				if let Some(declarations) = &m.tools_added {
					tools.extend(declarations.iter().map(|t| json!({"type":"function","function":{"name":t.name,"description":t.description,"parameters":t.parameters}})));
				}
			}
			Message::User(m) => {
				let content = match &m.content {
                    UserContent::Text(s) => json!(s),
                    UserContent::Blocks(blocks) => json!(blocks.iter().map(|b| match b {
                        TextOrImage::Text(t) => json!({"type":"text","text":t.text}),
                        TextOrImage::Image(i) => json!({"type":"image_url","image_url":{"url":format!("data:{};base64,{}",i.mime_type,i.data)}}),
                    }).collect::<Vec<_>>()),
                };
				messages.push(json!({"role":"user","content":content}));
			}
			Message::Assistant(m) => {
				let calls: Vec<_> = m.tool_calls().iter().map(|t| json!({"id":t.id,"type":"function","function":{"name":t.name,"arguments":t.arguments.to_string()}})).collect();
				let mut value = json!({"role":"assistant","content":if m.text().is_empty() { Value::Null } else { json!(m.text()) }});
				if !calls.is_empty() {
					value["tool_calls"] = json!(calls);
				}
				messages.push(value);
			}
			Message::ToolResult(m) => messages
				.push(json!({"role":"tool","tool_call_id":m.tool_call_id,"content":m.text()})),
		}
	}
	let mut body = json!({"model":model.id,"messages":messages,"stream":false});
	if !tools.is_empty() {
		body["tools"] = json!(tools);
	}
	if let Some(t) = options.temperature {
		body["temperature"] = json!(t);
	}
	if let Some(t) = options.max_tokens {
		body["max_completion_tokens"] = json!(t);
	}
	body
}

async fn request(
	model: &Model,
	context: &TranscriptContext,
	options: &StreamOptions,
) -> Result<Vec<Event>, String> {
	let key_env = if model.provider == "mimo" {
		"MIMO_API_KEY"
	} else {
		"OPENAI_API_KEY"
	};
	let key = options
		.api_key
		.clone()
		.or_else(|| std::env::var(key_env).ok())
		.filter(|s| !s.is_empty())
		.ok_or_else(|| format!("{key_env} not set"))?;
	let base = if model.base_url.is_empty() {
		"https://api.openai.com/v1"
	} else {
		model.base_url.trim_end_matches('/')
	};
	let response = reqwest::Client::new()
		.post(format!("{base}/chat/completions"))
		.bearer_auth(key)
		.json(&request_body(model, context, options))
		.send()
		.await
		.map_err(|e| format!("request failed: {e}"))?;
	let status = response.status();
	let value: Value = response
		.json()
		.await
		.map_err(|e| format!("invalid provider response ({status}): {e}"))?;
	if !status.is_success() {
		return Err(format!(
			"provider returned {status}: {}",
			value["error"]["message"]
				.as_str()
				.unwrap_or("request failed")
		));
	}
	response_events(model, &value)
}

fn response_events(model: &Model, value: &Value) -> Result<Vec<Event>, String> {
	let choice = value["choices"]
		.as_array()
		.and_then(|c| c.first())
		.ok_or("provider response contains no choices")?;
	let output = &choice["message"];
	let mut message = AssistantMessage::error(model, StopReason::Pending, "");
	message.content.clear();
	message.error_message = None;
	message.response_id = value["id"].as_str().map(str::to_string);
	message.response_model = value["model"].as_str().map(str::to_string);
	message.usage.input = value["usage"]["prompt_tokens"].as_u64().unwrap_or(0);
	message.usage.output = value["usage"]["completion_tokens"].as_u64().unwrap_or(0);
	message.usage.total_tokens = value["usage"]["total_tokens"]
		.as_u64()
		.unwrap_or(message.usage.input + message.usage.output);
	let finish = choice["finish_reason"].as_str().unwrap_or("stop");
	message.raw_stop_reason = Some(finish.to_string());
	message.stop_reason = match finish {
		"tool_calls" | "function_call" => StopReason::ToolUse,
		"length" => StopReason::Length,
		"stop" => StopReason::Stop,
		other => return Err(format!("unsupported finish reason: {other}")),
	};
	let mut events = vec![Event::Start {
		partial: message.clone(),
	}];
	if let Some(text) = output["content"].as_str().filter(|s| !s.is_empty()) {
		let index = message.content.len();
		message.content.push(AssistantContent::Text(TextContent {
			text: String::new(),
			text_signature: None,
		}));
		events.push(Event::TextStart {
			content_index: index,
			partial: message.clone(),
		});
		if let AssistantContent::Text(t) = &mut message.content[index] {
			t.text = text.to_string();
		}
		events.push(Event::TextDelta {
			content_index: index,
			delta: text.to_string(),
			partial: message.clone(),
		});
		events.push(Event::TextEnd {
			content_index: index,
			content: text.to_string(),
			partial: message.clone(),
		});
	}
	if let Some(calls) = output["tool_calls"].as_array() {
		for item in calls {
			let id = item["id"].as_str().ok_or("tool call missing id")?;
			let name = item["function"]["name"]
				.as_str()
				.ok_or("tool call missing name")?;
			let args = item["function"]["arguments"]
				.as_str()
				.ok_or("tool call missing arguments")?;
			let arguments = serde_json::from_str(args)
				.map_err(|e| format!("invalid tool arguments for {name}: {e}"))?;
			let call = ToolCall {
				id: id.into(),
				name: name.into(),
				arguments,
				thought_signature: None,
				namespace: None,
				arguments_error: None,
			};
			let index = message.content.len();
			message
				.content
				.push(AssistantContent::ToolCall(call.clone()));
			events.push(Event::ToolCallStart {
				content_index: index,
				partial: message.clone(),
			});
			events.push(Event::ToolCallEnd {
				content_index: index,
				tool_call: call,
				partial: message.clone(),
			});
		}
	}
	events.push(Event::Done {
		reason: message.stop_reason,
		message,
	});
	Ok(events)
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::types::{Tool, ToolResultMessage};
	#[test]
	fn serializes_tool_roundtrip() {
		let model = Model::new("test", "test", "openai-completions", "openai", "");
		let mut system = crate::types::SystemMessage::prompt("help");
		system.tools_added = Some(vec![Tool::new("ls", "list", json!({"type":"object"}))]);
		let response = json!({"choices":[{"finish_reason":"tool_calls","message":{"tool_calls":[{"id":"call_1","function":{"name":"ls","arguments":"{\"path\":\".\"}"}}]}}]});
		let events = response_events(&model, &response).unwrap();
		let Event::Done { message, .. } = events.last().unwrap() else {
			panic!()
		};
		let ctx = TranscriptContext {
			messages: vec![
				Message::System(system),
				Message::user("list"),
				Message::Assistant(message.clone()),
				Message::ToolResult(ToolResultMessage::text_result(
					"call_1", "ls", "file", false,
				)),
			],
		};
		let body = request_body(&model, &ctx, &StreamOptions::default());
		assert_eq!(body["tools"][0]["function"]["name"], "ls");
		assert_eq!(
			body["messages"][2]["tool_calls"][0]["function"]["arguments"],
			"{\"path\":\".\"}"
		);
		assert_eq!(body["messages"][3]["tool_call_id"], "call_1");
	}
	#[test]
	fn rejects_malformed_tool_arguments() {
		let model = Model::new("test", "test", "openai-completions", "openai", "");
		let v = json!({"choices":[{"finish_reason":"tool_calls","message":{"tool_calls":[{"id":"x","function":{"name":"ls","arguments":"{"}}]}}]});
		assert!(response_events(&model, &v).is_err());
	}
}
