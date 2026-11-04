//! Provider API adapters — port of `pi/packages/ai/src/api/*` and
//! `pi/packages/ai/src/providers/*`.
//!
//! Each adapter module owns one `KnownApi` wire format and exports a
//! [`StreamFn`] implementation. The registry maps `Model::api` to its adapter;
//! unknown APIs return an explicit error.

pub mod anthropic;
pub mod bedrock_converse_stream;
pub mod google_generative_ai;
pub mod google_vertex;
mod json_repair;
pub mod mistral_conversations;
pub mod mock;
pub mod openai_codex_responses;
pub mod openai_completions;
pub mod openai_responses;
pub mod pi_messages;

use std::sync::Arc;

use crate::model::{Api, Model};
use crate::stream::StreamFn;

/// Resolve the stream function for a model's API.
///
/// Resolve a supported adapter; unsupported APIs return an error.
pub fn stream_fn_for(model: &Model) -> StreamFn {
	match model.api.as_str() {
		Api::ANTHROPIC_MESSAGES => anthropic::stream_fn(),
		Api::OPENAI_COMPLETIONS => openai_completions::stream_fn(),
		Api::OPENAI_RESPONSES => openai_responses::stream_fn(),
		Api::OPENAI_CODEX_RESPONSES => openai_codex_responses::stream_fn(),
		Api::AZURE_OPENAI_RESPONSES => openai_responses::azure_stream_fn(),
		Api::GOOGLE_GENERATIVE_AI => google_generative_ai::stream_fn(),
		Api::GOOGLE_VERTEX => google_vertex::stream_fn(),
		Api::MISTRAL_CONVERSATIONS => mistral_conversations::stream_fn(),
		Api::BEDROCK_CONVERSE_STREAM => bedrock_converse_stream::stream_fn(),
		Api::PI_MESSAGES => pi_messages::stream_fn(),
		"mock" => Arc::new(mock::stream),
		_ => Arc::new(|model, _, _| {
			let error = crate::types::AssistantMessage::error(
				model,
				crate::types::StopReason::Error,
				format!("unsupported provider API: {}", model.api),
			);
			Box::pin(futures::stream::iter(vec![
				crate::stream::AssistantMessageEvent::Error {
					reason: crate::types::StopReason::Error,
					error,
				},
			]))
		}),
	}
}
