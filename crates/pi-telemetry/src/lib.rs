//! pi-telemetry — vendor-neutral telemetry contracts for pi-rs.
//!
//! Rust port of `pi/packages/telemetry`: typed span/error events that flow
//! through provider requests, agent runs, and tool executions without binding
//! to a concrete OTLP exporter.
//!
//! Framework stage: only the span contract pi-ai depends on is defined. The
//! full event schema, samplers, and exporters (OTLP, SQLite) land with the
//! durable/telemetry phase.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Explicit parent span for telemetry produced by a logical request.
///
/// Mirrors `TelemetrySpan` as consumed by `pi-ai` provider request options.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TelemetrySpan {
	/// Span name, e.g. `provider.request` or `agent.turn`.
	pub name: String,
	/// Trace id when part of a distributed trace.
	pub trace_id: Option<String>,
	/// Parent span id, when nested.
	pub parent_span_id: Option<String>,
	/// Free-form attributes.
	pub attributes: Value,
}

impl TelemetrySpan {
	pub fn new(name: impl Into<String>) -> Self {
		Self {
			name: name.into(),
			trace_id: None,
			parent_span_id: None,
			attributes: Value::Null,
		}
	}
}
