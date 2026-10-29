//! JSON event protocol — port of `pi/packages/coding-agent/src/modes/json-event.ts`.
//!
//! `toJsonEvent` strips the cumulative assistant snapshots from streaming wire
//! events: `message_update` events collapse to `{type, usage,
//! assistantMessageEvent}` with the `partial` field removed, and a
//! `toolcall_start` delta additionally gains `id`/`toolName` taken from the
//! message content at `contentIndex` (their size is constant, unlike
//! `partial`). Every other event passes through unchanged.
//!
//! Input here is the pi-rs [`SessionEvent`] rather than the upstream
//! `AgentSessionEvent` union; the frame mapping follows the camelCase patch
//! layer already established by `modes/rpc.rs` (`agent_event_frame`):
//!
//! - Agent events: identical envelope field renaming (`tool_call_id` →
//!   `toolCallId`, …); `message_update` deltas carry the upstream field names
//!   (`contentIndex`, `delta`, …) because `pi-agent`'s emit layer already
//!   produces them.
//! - `Settled` → the bare `{"type":"agent_settled"}` frame (upstream carries
//!   an `aborted` field the session layer does not track — same deviation as
//!   RPC mode).
//! - `Notice`/`Persisted` have no upstream `AgentSessionEvent` counterpart
//!   (notices route through extensions upstream); they return `None` and the
//!   print driver sends them to stderr instead of the JSON stream.

use serde_json::{Value, json};

use crate::core::agent_session::SessionEvent;

/// Rename only event envelope fields; tool payloads and arbitrary user JSON
/// are opaque and must not be recursively rewritten. Shared with
/// `modes/rpc.rs`'s `agent_event_frame` (kept local: rpc.rs is a separate
/// territory and its function is private).
fn agent_event_value(event: &pi_agent::AgentEvent) -> Value {
	let mut value = serde_json::to_value(event).expect("agent events serialize");
	let object = value.as_object_mut().expect("agent event is an object");
	for (source, target) in [
		("tool_results", "toolResults"),
		("tool_call_id", "toolCallId"),
		("tool_name", "toolName"),
		("partial_result", "partialResult"),
		("is_error", "isError"),
		("duration_ms", "durationMs"),
	] {
		if let Some(field) = object.remove(source) {
			object.insert(target.into(), field);
		}
	}
	value
}

/// `toJsonAssistantMessageEvent` (json-event.ts:20-38): drop `partial`; for a
/// `toolcall_start` delta add `id`/`toolName` from the tool call at
/// `contentIndex`.
///
/// Upstream throws when the content at `contentIndex` is not a tool call; the
/// port follows the tolerant rpc.rs patch-layer precedent instead (the delta
/// shape is already normalized by pi-agent's emit layer, so the condition is
/// not expected to fire) — the id/toolName pair is simply omitted.
fn to_json_assistant_message_event(message: &pi_ai::AssistantMessage, delta: &mut Value) {
	if delta["type"] == "toolcall_start" {
		let index = delta["contentIndex"].as_u64().unwrap_or(u64::MAX) as usize;
		if let Some(pi_ai::AssistantContent::ToolCall(call)) = message.content.get(index) {
			delta["id"] = json!(call.id);
			delta["toolName"] = json!(call.name);
		}
	}
	delta
		.as_object_mut()
		.expect("assistant event is an object")
		.remove("partial");
}

/// `toJsonEvent` (json-event.ts:48-61) over the pi-rs [`SessionEvent`] union:
/// returns the JSONL frame to write, or `None` for events that stay off the
/// JSON wire (notices → stderr, `Persisted` → internal bookkeeping).
pub fn to_json_event(event: &SessionEvent) -> Option<String> {
	match event {
		SessionEvent::Agent(agent_event) => {
			if let pi_agent::AgentEvent::MessageUpdate {
				message,
				assistant_message_event,
			} = agent_event.as_ref()
			{
				let pi_ai::Message::Assistant(message) = message else {
					// Upstream throws "message_update message is not an
					// assistant message"; the loop only emits assistant
					// updates, same invariant rpc.rs relies on.
					unreachable!("message updates are always assistant messages");
				};
				let mut delta = assistant_message_event.clone();
				to_json_assistant_message_event(message, &mut delta);
				return Some(
					json!({
						"type": "message_update",
						"usage": message.usage,
						"assistantMessageEvent": delta,
					})
					.to_string(),
				);
			}
			Some(agent_event_value(agent_event).to_string())
		}
		SessionEvent::Settled { aborted } => {
			Some(json!({"type": "agent_settled", "aborted": aborted}).to_string())
		}
		SessionEvent::Notice(_) | SessionEvent::Persisted { .. } => None,
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use serde_json::Value;

	fn parse(frame: &str) -> Value {
		serde_json::from_str(frame).expect("frames are JSON")
	}

	fn message_update(delta: Value, content: Vec<pi_ai::AssistantContent>) -> SessionEvent {
		let mut message = pi_ai::AssistantMessage {
			content,
			api: "mock".into(),
			provider: "mock".into(),
			model: "mock-echo".into(),
			response_model: None,
			response_id: None,
			thinking_level: None,
			usage: pi_ai::Usage {
				input: 11,
				output: 7,
				..pi_ai::Usage::default()
			},
			stop_reason: pi_ai::StopReason::Pending,
			error_message: None,
			raw_stop_reason: None,
			end_turn: None,
			timestamp: 0,
			duration_ms: None,
		};
		message.usage.total_tokens = 18;
		SessionEvent::Agent(Box::new(pi_agent::AgentEvent::MessageUpdate {
			message: pi_ai::Message::Assistant(message),
			assistant_message_event: delta,
		}))
	}

	#[test]
	fn message_update_drops_partial_and_keeps_usage() {
		let delta = json!({"type": "text_delta", "contentIndex": 0, "delta": "hi", "partial": {"role": "assistant"}});
		let event = message_update(delta, vec![]);
		let frame = parse(&to_json_event(&event).expect("json frame"));
		assert_eq!(frame["type"], "message_update");
		assert_eq!(frame["usage"]["input"], 11);
		assert_eq!(frame["usage"]["totalTokens"], 18);
		let inner = &frame["assistantMessageEvent"];
		assert_eq!(inner["type"], "text_delta");
		assert_eq!(inner["contentIndex"], 0);
		assert_eq!(inner["delta"], "hi");
		assert!(inner.get("partial").is_none(), "partial must be stripped");
		assert!(frame.get("message").is_none(), "snapshot must be stripped");
	}

	#[test]
	fn toolcall_start_gains_id_and_tool_name_from_content_index() {
		let call = pi_ai::ToolCall {
			id: "call-1".into(),
			name: "read".into(),
			arguments: json!({"path": "/tmp/x"}),
			thought_signature: None,
			namespace: None,
			arguments_error: None,
		};
		let delta = json!({"type": "toolcall_start", "contentIndex": 1, "partial": {"a": 1}});
		let event = message_update(
			delta,
			vec![
				pi_ai::AssistantContent::Text(pi_ai::TextContent {
					text: "thinking about it".into(),
					text_signature: None,
				}),
				pi_ai::AssistantContent::ToolCall(call),
			],
		);
		let frame = parse(&to_json_event(&event).expect("json frame"));
		let inner = &frame["assistantMessageEvent"];
		assert_eq!(inner["type"], "toolcall_start");
		assert_eq!(inner["id"], "call-1");
		assert_eq!(inner["toolName"], "read");
		assert!(inner.get("partial").is_none());
	}

	#[test]
	fn toolcall_start_without_a_tool_call_at_the_index_omits_the_pair() {
		// Upstream throws here; the port follows the rpc.rs tolerance and
		// omits id/toolName instead (declared deviation).
		let delta = json!({"type": "toolcall_start", "contentIndex": 5, "partial": {}});
		let event = message_update(delta, vec![]);
		let frame = parse(&to_json_event(&event).expect("json frame"));
		let inner = &frame["assistantMessageEvent"];
		assert_eq!(inner["type"], "toolcall_start");
		assert!(inner.get("id").is_none());
		assert!(inner.get("toolName").is_none());
	}

	#[test]
	fn non_message_update_agent_events_pass_through_with_camel_case_envelope() {
		let event = SessionEvent::Agent(Box::new(pi_agent::AgentEvent::ToolExecutionEnd {
			tool_call_id: "call-9".into(),
			tool_name: "bash".into(),
			result: json!({"user_key": 1}),
			is_error: false,
			duration_ms: Some(5),
		}));
		let frame = parse(&to_json_event(&event).expect("json frame"));
		assert_eq!(
			frame,
			json!({
				"type": "tool_execution_end",
				"toolCallId": "call-9",
				"toolName": "bash",
				"result": {"user_key": 1},
				"isError": false,
				"durationMs": 5,
			})
		);
	}

	#[test]
	fn settled_matches_the_upstream_frame() {
		let frame =
			parse(&to_json_event(&SessionEvent::Settled { aborted: false }).expect("json frame"));
		assert_eq!(frame, json!({"type": "agent_settled", "aborted": false}));
		let frame =
			parse(&to_json_event(&SessionEvent::Settled { aborted: true }).expect("json frame"));
		assert_eq!(frame, json!({"type": "agent_settled", "aborted": true}));
	}

	#[test]
	fn notices_and_persisted_stay_off_the_json_wire() {
		assert!(to_json_event(&SessionEvent::Notice("model → x".into())).is_none());
		assert!(
			to_json_event(&SessionEvent::Persisted { kind: "message" }).is_none(),
			"internal bookkeeping is not a wire event"
		);
	}
}
