//! Provider API adapters — port of `pi/packages/ai/src/api/*` and
//! `pi/packages/ai/src/providers/*`.
//!
//! Each adapter module owns one `KnownApi` wire format and exports a
//! [`StreamFn`] implementation. The registry maps `Model::api` to its adapter;
//! unknown APIs fall back to the mock adapter so the loop stays testable
//! before HTTP adapters land.

pub mod anthropic;
pub mod mock;
pub mod openai_completions;

use std::sync::Arc;

use crate::model::{Api, Model};
use crate::stream::StreamFn;

/// Resolve the stream function for a model's API.
///
/// HTTP adapters (anthropic, openai, google, ...) are framework-stage stubs;
/// the mock adapter fully implements the event protocol and is used whenever
/// no concrete adapter is available yet.
pub fn stream_fn_for(model: &Model) -> StreamFn {
	match model.api.as_str() {
		Api::ANTHROPIC_MESSAGES => anthropic::stream_fn(),
		Api::OPENAI_COMPLETIONS => openai_completions::stream_fn(),
		_ => Arc::new(mock::stream),
	}
}
