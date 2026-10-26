//! OpenAI Chat Completions API adapter — port of
//! `pi/packages/ai/src/api/openai-completions.ts`.
//!
//! Framework stage: wire format (SSE chunks, tool_calls deltas, reasoning
//! content, compat overrides for llama.cpp/vLLM/SGLang) is not implemented
//! yet; the adapter fails fast with a protocol-correct error event.

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
				AssistantMessage::error(model, StopReason::Error, "OPENAI_API_KEY not set")
			} else {
				AssistantMessage::error(
					model,
					StopReason::Error,
					"openai-completions adapter not implemented yet (pi-rs framework stage)",
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
