//! Auto-retry policy — port of the retry surface pi-rs needs from
//! `pi/packages/ai/src/utils/retry.ts` (`retryDelayMs`,
//! `isRetryableAssistantError`, `RetryPolicy`) plus the agent-session glue
//! (`_isRetryableError` in `core/agent-session.ts:3769`).
//!
//! Defaults come from `SETTINGS_DEFAULTS.retry` (`core/settings-defaults.ts`):
//! `enabled: true`, `maxRetries: 3`, `baseDelayMs: 2000`,
//! `maxAgentDelayMs: 60_000`. pi-rs hardcodes them here because the settings
//! schema (`core/settings_manager.rs`) does not carry a `retry` key yet — the
//! runtime `set_auto_retry` toggle covers the enabled flag; wiring the
//! remaining knobs through settings lands with the settings-manager expansion.
//!
//! Pattern reduction (accepted, mirrors `OVERFLOW_PATTERNS` in
//! `core/agent_session.rs`): upstream classifies errors with two regexes;
//! this port lowers them to case-insensitive substring lists. Regex
//! alternatives that differ only by optional separators
//! (`rate.?limit`, `server.?error`) expand to their common spellings, and the
//! HTTP status alternatives keep upstream's word-boundary rule
//! (`(?<![a-z0-9])(?:429|500|...)(?![a-z0-9])`).

use pi_ai::{AssistantMessage, StopReason};

/// `SETTINGS_DEFAULTS.retry.enabled` — auto-retry starts enabled.
pub const RETRY_ENABLED_DEFAULT: bool = true;
/// `SETTINGS_DEFAULTS.retry.maxRetries`.
pub const RETRY_MAX_RETRIES: u32 = 3;
/// `SETTINGS_DEFAULTS.retry.baseDelayMs`.
pub const RETRY_BASE_DELAY_MS: u64 = 2000;
/// `DEFAULT_MAX_AGENT_RETRY_DELAY_MS` (`pi-ai/utils/retry.ts`) —
/// `SETTINGS_DEFAULTS.retry.maxAgentDelayMs`.
pub const RETRY_MAX_AGENT_DELAY_MS: u64 = 60_000;

/// `retryDelayMs` upstream: `baseDelayMs * 2^(attempt-1)`, capped at
/// `maxAgentDelayMs` (60s). `attempt` is 1-based. The `_with` variant takes
/// the effective base (tests shorten it; see `RetryRuntime`).
pub fn retry_delay_with(base_delay_ms: u64, attempt: u32) -> u64 {
	let delay = base_delay_ms.saturating_mul(1u64 << attempt.saturating_sub(1).min(63));
	delay.min(RETRY_MAX_AGENT_DELAY_MS)
}

pub fn retry_delay_ms(attempt: u32) -> u64 {
	retry_delay_with(RETRY_BASE_DELAY_MS, attempt)
}

/// Errors that look retryable but are really account/subscription limits —
/// `NON_RETRYABLE_PROVIDER_LIMIT_ERROR_PATTERN` upstream.
const NON_RETRYABLE_LIMIT_PATTERNS: &[&str] = &[
	"gousagelimiterror",
	"freeusagelimiterror",
	"monthly usage limit reached",
	"available balance",
	"insufficient_quota",
	"out of budget",
	"quota exceeded",
	"billing",
	"subscription_sharing_usage_limit_exceeded",
	"subscription_sharing_workplace_individual_usage_limit_exceeded",
];

/// Transient provider/network failures worth a retry —
/// `RETRYABLE_PROVIDER_ERROR_PATTERN` upstream, reduced to substrings.
const RETRYABLE_ERROR_PATTERNS: &[&str] = &[
	// Load and capacity wording.
	"overloaded",
	"server_busy",
	"servers are currently busy",
	"currently experiencing high demand",
	"model is at capacity",
	// `rate.?limit`: separated and joined spellings.
	"rate limit",
	"rate-limit",
	"ratelimit",
	"too many requests",
	// `service.?(is.?)?(temporarily.?)?unavailable`.
	"service unavailable",
	"service is unavailable",
	"service temporarily unavailable",
	// `server.?error` / `internal.?error`.
	"server error",
	"servererror",
	"internal error",
	"internalerror",
	// Wrapper/gateway relays of upstream failures.
	"provider returned error",
	"provider returned an error",
	"exceeded request buffer limit while retrying upstream",
	"unable to forward upstream",
	"upstream request failed",
	"upstream_request_failed",
	"upstream_stream_read_error",
	"stream was interrupted",
	"http/2 stream failed",
	// Network, proxy, and transport failures.
	"network error",
	"network_error",
	"connection error",
	"connection_error",
	"connection refused",
	"connection lost",
	"other side closed",
	"fetch failed",
	"getaddrinfo",
	"enotfound",
	"eai_again",
	"upstream connect",
	"reset before headers",
	"socket hang up",
	"socket connection was closed",
	"timed out",
	"timedout",
	"timeout",
	"terminated",
	// WebSocket transports.
	"websocket closed",
	"websocketclosed",
	"websocket error",
	"websocketerror",
	// Premature stream endings.
	"ended without",
	"stream ended before message_stop",
	"stream ended before a terminal response event",
	"http2 request did not get a response",
	"stream disconnected before completion",
	"pending stream has been canceled",
	// Provider-requested retry delay cap failures.
	"retry delay",
	// Explicit mid-stream retry guidance.
	"you can retry your request",
	"try your request again",
	"please retry your request",
	// gRPC providers (NVIDIA NIM).
	"resourceexhausted",
	// Sign in with ChatGPT transient usage failures.
	"subscription_sharing_usage_unavailable",
	"subscription_sharing_user_unavailable",
];

/// HTTP status codes upstream matches with word boundaries.
const RETRYABLE_STATUS_CODES: &[u16] = &[429, 500, 502, 503, 504, 520, 524];

/// `isRetryableAssistantError` upstream: an error stop whose message matches
/// a retryable provider pattern and none of the subscription-limit patterns.
pub fn is_retryable_assistant_error(message: &AssistantMessage) -> bool {
	if message.stop_reason != StopReason::Error {
		return false;
	}
	let Some(error) = message.error_message.as_deref() else {
		return false;
	};
	let lower = error.to_lowercase();
	if NON_RETRYABLE_LIMIT_PATTERNS
		.iter()
		.any(|p| lower.contains(p))
	{
		return false;
	}
	if RETRYABLE_ERROR_PATTERNS.iter().any(|p| lower.contains(p)) {
		return true;
	}
	RETRYABLE_STATUS_CODES
		.iter()
		.any(|code| contains_status_code(&lower, *code))
}

/// Upstream's status-code rule: the digits must not sit inside a longer
/// alphanumeric run (`(?<![a-z0-9])…(?![a-z0-9])`).
fn contains_status_code(lower: &str, code: u16) -> bool {
	let needle = code.to_string();
	let mut start = 0;
	while let Some(position) = lower[start..].find(&needle) {
		let absolute = start + position;
		let end = absolute + needle.len();
		let before = lower[..absolute].chars().next_back();
		let after = lower[end..].chars().next();
		let boundary =
			|character: Option<char>| !character.is_some_and(|c| c.is_ascii_alphanumeric());
		if boundary(before) && boundary(after) {
			return true;
		}
		start = end;
	}
	false
}

/// `_isRetryableError` upstream (`agent-session.ts:3769`): retryable provider
/// errors except context overflow, which the compaction recovery owns.
pub fn is_retryable_error(message: &AssistantMessage, context_window: u64) -> bool {
	if super::agent_session::is_context_overflow(message, context_window) {
		return false;
	}
	is_retryable_assistant_error(message)
}

#[cfg(test)]
mod tests {
	use super::*;
	use pi_ai::AssistantMessage;

	fn error_message(text: &str) -> AssistantMessage {
		let mut message = AssistantMessage::error(
			&pi_ai::Model::new("m", "m", "openai-completions", "m", "http://localhost"),
			StopReason::Error,
			text,
		);
		message.stop_reason = StopReason::Error;
		message
	}

	#[test]
	fn delay_formula_matches_upstream_backoff() {
		assert_eq!(retry_delay_ms(1), 2000);
		assert_eq!(retry_delay_ms(2), 4000);
		assert_eq!(retry_delay_ms(3), 8000);
		assert_eq!(retry_delay_ms(4), 16000);
		assert_eq!(retry_delay_ms(10), 60_000, "capped at maxAgentDelayMs");
		assert_eq!(retry_delay_ms(100), 60_000);
	}

	#[test]
	fn classifies_transient_errors_as_retryable() {
		for text in [
			"fetch failed: connection refused",
			"Anthropic API error (529): overloaded",
			"request failed with status 503",
			"The model is currently experiencing high demand",
			"stream ended before message_stop",
			"socket hang up",
			"request timed out after 30000ms",
		] {
			assert!(
				is_retryable_error(&error_message(text), 0),
				"`{text}` is retryable"
			);
		}
	}

	#[test]
	fn classifies_quota_and_overflow_as_not_retryable() {
		for text in [
			"insufficient_quota: you have exceeded your billing quota",
			"GoUsageLimitError: monthly usage limit reached",
			"prompt is too long: 213462 tokens > 200000 maximum",
			"context_length_exceeded",
		] {
			assert!(
				!is_retryable_error(&error_message(text), 0),
				"`{text}` is not retryable"
			);
		}
	}

	#[test]
	fn status_codes_respect_word_boundaries() {
		assert!(contains_status_code("error 502 from gateway", 502));
		assert!(contains_status_code("502", 502));
		assert!(
			!contains_status_code("id 15023", 502),
			"embedded digits do not match"
		);
		assert!(!contains_status_code("a502b", 502));
	}

	#[test]
	fn non_error_stops_are_never_retryable() {
		let mut message = error_message("overloaded");
		message.stop_reason = StopReason::Stop;
		assert!(!is_retryable_assistant_error(&message));
		message.stop_reason = StopReason::Length;
		assert!(!is_retryable_assistant_error(&message));
	}
}
