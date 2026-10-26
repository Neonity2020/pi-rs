//! Anthropic Messages API adapter — port of
//! `pi/packages/ai/src/api/anthropic-messages.ts`.
//!
//! Framework stage: wire format (SSE `message_start` / `content_block_*` /
//! `message_delta`, prompt-cache breakpoints, thinking budget mapping) is not
//! implemented yet. Until then the adapter fails fast with a protocol-correct
//! error event so the agent loop can surface it.

use std::sync::Arc;

use crate::model::Model;
use crate::stream::{AssistantMessageEvent, AssistantMessageEventStream, StreamFn, StreamOptions, TranscriptContext};
use crate::types::{AssistantMessage, StopReason};

pub fn stream_fn() -> StreamFn {
	Arc::new(
		|model: &Model,
		 _context: &TranscriptContext,
		 options: StreamOptions|
		 -> AssistantMessageEventStream {
			let error = if options.api_key.is_none() {
				AssistantMessage::error(model, StopReason::Error, "ANTHROPIC_API_KEY not set")
			} else {
				AssistantMessage::error(
					model,
					StopReason::Error,
					"anthropic-messages adapter not implemented yet (pi-rs framework stage); falling back requires the mock provider",
				)
			};
			let reason = StopReason::Error;
			Box::pin(futures::stream::iter(vec![AssistantMessageEvent::Error {
				reason,
				error,
			}]))
		},
	)
}
