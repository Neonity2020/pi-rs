//! Amazon Bedrock ConverseStream API adapter — port of the streaming `stream()`
//! in `pi/packages/ai/src/api/bedrock-converse-stream.ts` (first slice).
//!
//! Unlike the other adapters this is not SSE: the AWS SDK wraps a REST-JSON
//! `POST /model/{modelId}/converse-stream` request in SigV4 (or bearer-token)
//! auth and decodes the binary event-stream framing
//! (`application/vnd.amazon.eventstream`) whose JSON payloads carry the
//! ConverseStream events (`messageStart`, `contentBlockStart/Delta/Stop`,
//! `messageStop`, `metadata`). This port hand-rolls both halves in pure Rust:
//!
//! - **SigV4** (`sign_request`) — canonical request over host + x-amz-date +
//!   optional session token, HMAC-SHA256 key derivation; verified against the
//!   AWS documentation reference vector. Bearer tokens
//!   (`options.api_key` / `AWS_BEARER_TOKEN_BEDROCK`) and
//!   `AWS_BEDROCK_SKIP_AUTH=1` skip signing. Credentials come from
//!   `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY`/`AWS_SESSION_TOKEN`; the
//!   SDK's profile-file/SSO/IMDS credential chain is not implemented and fails
//!   with an explicit error (like the deferred GCE metadata-server in
//!   `google_vertex.rs`).
//! - **Event-stream decoder** (`EsDecoder`) — prelude/header/payload framing
//!   with CRC-32 validation, `:message-type`/`:exception-type` header routing.
//!
//! Region resolution ports the upstream order: ARN-embedded region >
//! `AWS_REGION`/`AWS_DEFAULT_REGION` > standard-endpoint host region >
//! `us-east-1` when no ambient `AWS_PROFILE`. A custom `model.base_url` that
//! is not a standard Bedrock host is always used as the endpoint root.
//!
//! Not modeled (declared deferrals): HTTP/2 (reqwest speaks HTTP/1.1, which
//! Bedrock accepts), proxy agents, `thinkingDisplay`/`interleavedThinking`
//! request options (upstream defaults apply: summarized + interleaved beta for
//! budget-thinking Claude), `requestMetadata`, `onPayload`/`onResponse` hooks,
//! `toolChoice` (absent from [`StreamOptions`]), strict-mode schemas
//! (`compat.supportsStrictMode`), cost calculation (zeroed like the other
//! adapters) and `sanitizeSurrogates` (Rust `String`s cannot carry lone
//! surrogates).

use std::collections::HashMap;
use std::sync::Arc;

use base64::Engine as _;
use hmac::{Hmac, Mac};
use reqwest::Url;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc::Sender;

use super::openai_responses::ToolArgsStream;
use crate::model::Model;
use crate::stream::{
	AssistantMessageEvent as Event, AssistantMessageEventStream, StreamFn, StreamOptions,
	TranscriptContext,
};
use crate::types::{
	AssistantContent, AssistantMessage, ImageContent, Message, StopReason, TextOrImage,
	ThinkingContent, ThinkingLevel, ToolCall, Usage, now_ms,
};

/// Events buffered for the consumer before the producer task backpressures.
const EVENT_CHANNEL_CAPACITY: usize = 64;

/// Placeholder Bedrock requires instead of empty text blocks.
const EMPTY_TEXT_PLACEHOLDER: &str = "<empty>";

/// Matches the placeholder the Anthropic API path uses for redacted thinking.
const REDACTED_THINKING_PLACEHOLDER: &str = "[Reasoning redacted]";

const THINKING_BINDING_CONTROLS_BETA: &str = "thinking-binding-controls-2026-08-01";

/// Human-readable prefixes for Bedrock SDK exception names; the downstream
/// retry logic matches patterns like `server.?error`, so the legacy wording is
/// preserved rather than the raw exception name.
const BEDROCK_ERROR_PREFIXES: [(&str, &str); 5] = [
	("InternalServerException", "Internal server error"),
	("ModelStreamErrorException", "Model stream error"),
	("ValidationException", "Validation error"),
	("ThrottlingException", "Throttling error"),
	("ServiceUnavailableException", "Service unavailable"),
];

const BEDROCK_DATA_RETENTION_DOCS_URL: &str =
	"https://docs.aws.amazon.com/bedrock/latest/userguide/data-retention.html";

const ERROR_TOOL_IMAGE_PLACEHOLDER: &str = "(image attached after the tool results)";

pub fn stream_fn() -> StreamFn {
	Arc::new(
		|model: &Model,
		 context: &TranscriptContext,
		 options: StreamOptions|
		 -> AssistantMessageEventStream { spawn_stream(model.clone(), context.clone(), options) },
	)
}

/// Runs the request on a task and forwards events through a channel, so the
/// returned stream yields deltas while the HTTP body is still arriving.
fn spawn_stream(
	model: Model,
	context: TranscriptContext,
	options: StreamOptions,
) -> AssistantMessageEventStream {
	let (tx, rx) = tokio::sync::mpsc::channel::<Event>(EVENT_CHANNEL_CAPACITY);
	tokio::spawn(async move {
		let signal = options.signal.clone();
		let result = tokio::select! {
			_ = signal.cancelled() => Err(StreamFailure {
				reason: StopReason::Aborted,
				message: "aborted by user".to_string(),
				session: None,
			}),
			result = run_stream(&model, &context, &options, &tx) => result,
		};
		if let Err(failure) = result {
			let error = match failure.session {
				Some(mut message) => {
					message.stop_reason = failure.reason;
					message.error_message = Some(failure.message);
					message
				}
				None => Box::new(AssistantMessage::error(
					&model,
					failure.reason,
					failure.message,
				)),
			};
			let _ = tx
				.send(Event::Error {
					reason: failure.reason,
					error: *error,
				})
				.await;
		}
	});
	Box::pin(futures::stream::unfold(rx, |mut rx| async move {
		rx.recv().await.map(|event| (event, rx))
	}))
}

/// A terminal stream failure. `session` carries the partially accumulated
/// assistant message when the failure happened mid-stream, mirroring the TS
/// catch block that reports the live output with an error stop reason.
struct StreamFailure {
	reason: StopReason,
	message: String,
	session: Option<Box<AssistantMessage>>,
}

async fn run_stream(
	model: &Model,
	context: &TranscriptContext,
	options: &StreamOptions,
	tx: &Sender<Event>,
) -> Result<(), StreamFailure> {
	let response = open_response(model, context, options)
		.await
		.map_err(|message| StreamFailure {
			reason: StopReason::Error,
			message,
			session: None,
		})?;
	let mut session = StreamSession::new(model);
	let mut decoder = EsDecoder::default();
	let mut chunk_stream = response;
	// Abort is handled by the task-level select in `spawn_stream`, mirroring
	// the other adapters; upstream re-checks the signal after the loop.
	loop {
		match chunk_stream.chunk().await {
			Ok(Some(chunk)) => {
				let mut frames = Vec::new();
				decoder
					.feed(&chunk, &mut frames)
					.map_err(|message| StreamFailure {
						reason: StopReason::Error,
						message,
						session: Some(Box::new(session.message.clone())),
					})?;
				if !forward_frames(&mut session, frames, tx).await? {
					return Ok(());
				}
			}
			Ok(None) => break,
			Err(error) => {
				return Err(StreamFailure {
					reason: StopReason::Error,
					message: format!("stream read failed: {error}"),
					session: Some(Box::new(session.message.clone())),
				});
			}
		}
	}
	// Flush a trailing partial frame; a truncated message is a stream error.
	if !decoder.buffer.is_empty() {
		return Err(StreamFailure {
			reason: StopReason::Error,
			message: "Bedrock event stream ended mid-frame".to_string(),
			session: Some(Box::new(session.message.clone())),
		});
	}

	// Tail checks of the upstream post-loop guards.
	if session.message.stop_reason == StopReason::Pending {
		return Err(StreamFailure {
			reason: StopReason::Error,
			message: "Bedrock stream ended without a stop reason".to_string(),
			session: Some(Box::new(session.message.clone())),
		});
	}
	if session.message.stop_reason == StopReason::Error
		|| session.message.stop_reason == StopReason::Aborted
	{
		return Err(StreamFailure {
			reason: StopReason::Error,
			message: session
				.message
				.error_message
				.clone()
				.unwrap_or_else(|| "An unknown error occurred".to_string()),
			session: Some(Box::new(session.message.clone())),
		});
	}
	// A stream can settle without stopping every block; finalize the rest.
	session.flush_open_blocks();
	let done_event = Event::Done {
		reason: session.message.stop_reason,
		message: session.into_message(),
	};
	forward(tx, vec![done_event]).await;
	Ok(())
}

/// Parses and forwards one batch of event-stream frames. Returns `Ok(false)`
/// when the consumer dropped the stream; the producer task then stops quietly.
async fn forward_frames(
	session: &mut StreamSession,
	frames: Vec<EsFrame>,
	tx: &Sender<Event>,
) -> Result<bool, StreamFailure> {
	for frame in frames {
		let events = session
			.process_frame(&frame)
			.map_err(|message| StreamFailure {
				reason: StopReason::Error,
				message,
				session: Some(Box::new(session.message.clone())),
			})?;
		if !forward(tx, events).await {
			return Ok(false);
		}
	}
	Ok(true)
}

/// Sends events to the consumer. Returns false once the receiver is gone.
async fn forward(tx: &Sender<Event>, events: Vec<Event>) -> bool {
	for event in events {
		if tx.send(event).await.is_err() {
			return false;
		}
	}
	true
}

// ---------------------------------------------------------------------------
// HTTP request
// ---------------------------------------------------------------------------

async fn open_response(
	model: &Model,
	context: &TranscriptContext,
	options: &StreamOptions,
) -> Result<reqwest::Response, String> {
	let region = resolve_region(model);
	let root = endpoint_root(model, &region);
	let url = format!(
		"{root}/model/{}/converse-stream",
		encode_path_segment(&model.id)
	);
	let parsed =
		Url::parse(&url).map_err(|error| format!("invalid endpoint URL {url}: {error}"))?;

	let body = serde_json::to_vec(&request_body(model, context, options))
		.map_err(|error| format!("serialize request body: {error}"))?;
	let headers = request_headers(options, &parsed, &body, &region)?;

	let mut request = reqwest::Client::new().post(url).body(body);
	for (name, value) in headers {
		request = request.header(name, value);
	}
	let response = request
		.header("accept", "application/vnd.amazon.eventstream")
		.send()
		.await
		.map_err(|error| format!("request failed: {error}"))?;
	let status = response.status();
	if !status.is_success() {
		let body = response.text().await.unwrap_or_default();
		// Surface the raw HTTP body (with status) so a gateway 403 does not
		// collapse into an opaque error, matching `formatBedrockError`.
		let core = if body.trim().is_empty() {
			"request failed".to_string()
		} else {
			format!("{status}: {}", body.trim())
		};
		return Err(with_data_retention_hint(core));
	}
	Ok(response)
}

/// SigV4 or bearer-token headers for one request.
fn request_headers(
	options: &StreamOptions,
	url: &Url,
	body: &[u8],
	region: &str,
) -> Result<Vec<(String, String)>, String> {
	let (amz_date, _datestamp) = amz_dates(now_ms());
	let skip_auth = std::env::var("AWS_BEDROCK_SKIP_AUTH").ok().as_deref() == Some("1");
	let bearer = if skip_auth {
		None
	} else {
		options
			.api_key
			.clone()
			.filter(|key| !key.is_empty())
			.or_else(|| {
				std::env::var("AWS_BEARER_TOKEN_BEDROCK")
					.ok()
					.filter(|k| !k.is_empty())
			})
	};
	if let Some(token) = bearer {
		return Ok(vec![
			("x-amz-date".to_string(), amz_date),
			("authorization".to_string(), format!("Bearer {token}")),
		]);
	}

	let (access_key, secret_key, session_token) = resolve_sigv4_credentials()?;
	let host = canonical_host(url);
	let signer = SigV4 {
		access_key,
		secret_key,
		session_token,
		region: region.to_string(),
		service: "bedrock".to_string(),
	};
	Ok(signer.sign_request("POST", url.path(), body, &host, &amz_date))
}

/// Ambient AWS credentials for SigV4; the SDK's profile-file/SSO/IMDS chain is
/// a declared deferral, so a missing pair is an explicit configuration error.
fn resolve_sigv4_credentials() -> Result<(String, String, Option<String>), String> {
	let access_key = std::env::var("AWS_ACCESS_KEY_ID")
		.ok()
		.filter(|k| !k.is_empty());
	let secret_key = std::env::var("AWS_SECRET_ACCESS_KEY")
		.ok()
		.filter(|k| !k.is_empty());
	match (access_key, secret_key) {
		(Some(access), Some(secret)) => {
			let session_token = std::env::var("AWS_SESSION_TOKEN")
				.ok()
				.filter(|token| !token.is_empty());
			Ok((access, secret, session_token))
		}
		_ => Err(
			"AWS credentials not configured: set AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY, \
			 or provide a bearer token via the provider API key or AWS_BEARER_TOKEN_BEDROCK \
			 (AWS profile files and the instance metadata service are not supported)"
				.to_string(),
		),
	}
}

/// Region for the endpoint — port of the upstream resolution order.
fn resolve_region(model: &Model) -> String {
	// ARN-embedded region wins so AWS_REGION set for other services is ignored.
	if let Some(captures) = arn_region(&model.id) {
		return captures;
	}
	if let Ok(region) = std::env::var("AWS_REGION")
		&& !region.is_empty()
	{
		return region;
	}
	if let Ok(region) = std::env::var("AWS_DEFAULT_REGION")
		&& !region.is_empty()
	{
		return region;
	}
	if let Some(region) = standard_endpoint_region(&model.base_url) {
		return region;
	}
	"us-east-1".to_string()
}

/// `arn:aws:bedrock:{region}:...` prefix of a model id.
fn arn_region(model_id: &str) -> Option<String> {
	let rest = model_id.strip_prefix("arn:aws")?;
	let rest = rest.strip_prefix(|c: char| c == ':' || c == '-')?;
	// Partition handles both `arn:aws:bedrock:...` and `arn:aws-us-gov:...`.
	let after = rest.find("bedrock:")?;
	let tail = &rest[after + "bedrock:".len()..];
	let region = tail.split(':').next()?;
	if region.is_empty() {
		None
	} else {
		Some(region.to_string())
	}
}

/// Region encoded in a standard `bedrock-runtime[-fips].{region}.amazonaws.com[.cn]` host.
fn standard_endpoint_region(base_url: &str) -> Option<String> {
	let host = Url::parse(base_url).ok()?.host_str()?.to_lowercase();
	let suffixes = [".amazonaws.com.cn", ".amazonaws.com"];
	for suffix in suffixes {
		if let Some(stripped) = host.strip_suffix(suffix) {
			let region = if let Some(region) = stripped.strip_prefix("bedrock-runtime-fips.") {
				region
			} else if let Some(region) = stripped.strip_prefix("bedrock-runtime.") {
				region
			} else {
				continue;
			};
			return Some(region.to_string());
		}
	}
	None
}

/// Whether the configured `base_url` is a standard Bedrock host whose region
/// matches the resolved one; non-standard hosts are always used verbatim.
fn endpoint_root(model: &Model, region: &str) -> String {
	if model.base_url.is_empty() {
		return format!("https://bedrock-runtime.{region}.amazonaws.com");
	}
	if standard_endpoint_region(&model.base_url).is_some() {
		return format!("https://bedrock-runtime.{region}.amazonaws.com");
	}
	model.base_url.trim_end_matches('/').to_string()
}

fn canonical_host(url: &Url) -> String {
	match url.port() {
		Some(port) => format!("{}:{port}", url.host_str().unwrap_or_default()),
		None => url.host_str().unwrap_or_default().to_string(),
	}
}

// ---------------------------------------------------------------------------
// SigV4 signing
// ---------------------------------------------------------------------------

struct SigV4 {
	access_key: String,
	secret_key: String,
	session_token: Option<String>,
	region: String,
	service: String,
}

impl SigV4 {
	/// Canonical request + derived `Authorization` headers. Verified against
	/// the AWS SigV4 documentation reference vector (`sigv4_reference_vector`).
	fn sign_request(
		&self,
		method: &str,
		canonical_path: &str,
		body: &[u8],
		host: &str,
		amz_date: &str,
	) -> Vec<(String, String)> {
		let datestamp = &amz_date[..8];
		let payload_hash = hex_sha256(body);

		let mut canonical_headers = format!("host:{host}\nx-amz-date:{amz_date}\n");
		let mut signed_headers = "host;x-amz-date".to_string();
		if let Some(token) = &self.session_token {
			canonical_headers.push_str(&format!("x-amz-security-token:{token}\n"));
			signed_headers.push_str(";x-amz-security-token");
		}

		let canonical_request = format!(
			"{method}\n{canonical_path}\n\n{canonical_headers}\n{signed_headers}\n{payload_hash}"
		);
		let scope = format!("{datestamp}/{}/{}/aws4_request", self.region, self.service);
		let string_to_sign = format!(
			"AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
			hex_sha256(canonical_request.as_bytes())
		);

		let k_date = hmac_sha256(
			format!("AWS4{}", self.secret_key).as_bytes(),
			datestamp.as_bytes(),
		);
		let k_region = hmac_sha256(&k_date, self.region.as_bytes());
		let k_service = hmac_sha256(&k_region, self.service.as_bytes());
		let k_signing = hmac_sha256(&k_service, b"aws4_request");
		let signature = hex(&hmac_sha256(&k_signing, string_to_sign.as_bytes()));

		let authorization = format!(
			"AWS4-HMAC-SHA256 Credential={}/{}, SignedHeaders={}, Signature={}",
			self.access_key, scope, signed_headers, signature
		);
		let mut headers = vec![
			("x-amz-date".to_string(), amz_date.to_string()),
			("authorization".to_string(), authorization),
		];
		if let Some(token) = &self.session_token {
			headers.push(("x-amz-security-token".to_string(), token.clone()));
		}
		headers
	}
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
	let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts any key length");
	mac.update(data);
	mac.finalize().into_bytes().to_vec()
}

fn hex_sha256(data: &[u8]) -> String {
	let digest = Sha256::digest(data);
	hex(&digest)
}

fn hex(bytes: &[u8]) -> String {
	bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// `(x-amz-date, datestamp)` in SigV4's UTC formats, without a calendar crate.
fn amz_dates(epoch_ms: u64) -> (String, String) {
	let total_seconds = epoch_ms / 1000;
	let days = (total_seconds / 86_400) as i64;
	let seconds_of_day = total_seconds % 86_400;
	let (year, month, day) = civil_from_days(days);
	let (hour, minute, second) = (
		seconds_of_day / 3600,
		(seconds_of_day % 3600) / 60,
		seconds_of_day % 60,
	);
	(
		format!("{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}Z"),
		format!("{year:04}{month:02}{day:02}"),
	)
}

/// Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
	let z = days + 719_468;
	let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
	let doe = z - era * 146_097;
	let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
	let year = yoe + era * 400;
	let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
	let mp = (5 * doy + 2) / 153;
	let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
	let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
	let year = if month <= 2 { year + 1 } else { year };
	(year, month, day)
}

/// Percent-encode a path segment the way the AWS SDK does (RFC3986 unreserved
/// characters stay literal, everything else — including `:` and `/` — is
/// escaped; the server decodes the sequence back).
fn encode_path_segment(segment: &str) -> String {
	let mut out = String::with_capacity(segment.len());
	for byte in segment.as_bytes() {
		match byte {
			b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
				out.push(*byte as char)
			}
			other => out.push_str(&format!("%{other:02X}")),
		}
	}
	out
}

// ---------------------------------------------------------------------------
// Binary event-stream decoding
// ---------------------------------------------------------------------------

/// One decoded event-stream message: routing headers plus the JSON payload.
struct EsFrame {
	message_type: String,
	event_type: Option<String>,
	exception_type: Option<String>,
	error_message: Option<String>,
	payload: Vec<u8>,
}

/// Incremental binary event-stream reader — buffers body bytes and emits one
/// `EsFrame` per complete message (prelude CRC and message CRC validated).
#[derive(Default)]
struct EsDecoder {
	buffer: Vec<u8>,
}

impl EsDecoder {
	fn feed(&mut self, chunk: &[u8], frames: &mut Vec<EsFrame>) -> Result<(), String> {
		self.buffer.extend_from_slice(chunk);
		while self.buffer.len() >= 12 {
			let total_len = u32::from_be_bytes([
				self.buffer[0],
				self.buffer[1],
				self.buffer[2],
				self.buffer[3],
			]) as usize;
			let headers_len = u32::from_be_bytes([
				self.buffer[4],
				self.buffer[5],
				self.buffer[6],
				self.buffer[7],
			]) as usize;
			// 12-byte prelude + headers + payload + 4-byte message CRC.
			if total_len < 12 + headers_len + 4 {
				return Err(format!(
					"invalid event-stream frame: total={total_len} headers={headers_len}"
				));
			}
			if total_len > 16 * 1024 * 1024 {
				return Err(format!("event-stream frame too large: {total_len} bytes"));
			}
			if self.buffer.len() < total_len {
				break;
			}
			let frame_bytes: Vec<u8> = self.buffer.drain(..total_len).collect();
			frames.push(decode_frame(&frame_bytes)?);
		}
		Ok(())
	}
}

fn decode_frame(bytes: &[u8]) -> Result<EsFrame, String> {
	let total_len = bytes.len();
	let headers_len = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
	let prelude_crc = u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
	let message_crc = u32::from_be_bytes([
		bytes[total_len - 4],
		bytes[total_len - 3],
		bytes[total_len - 2],
		bytes[total_len - 1],
	]);
	if crc32(&bytes[..8]) != prelude_crc {
		return Err("event-stream prelude CRC mismatch".to_string());
	}
	if crc32(&bytes[..total_len - 4]) != message_crc {
		return Err("event-stream message CRC mismatch".to_string());
	}
	let headers = parse_headers(&bytes[12..12 + headers_len])?;
	let payload = bytes[12 + headers_len..total_len - 4].to_vec();
	Ok(EsFrame {
		message_type: headers
			.get(":message-type")
			.cloned()
			.unwrap_or_else(|| "event".to_string()),
		event_type: headers.get(":event-type").cloned(),
		exception_type: headers.get(":exception-type").cloned(),
		error_message: headers.get(":error-message").cloned(),
		payload,
	})
}

/// Parses typed name-value header pairs; values are kept only when string-
/// typed (`:message-type` etc. are all strings), other types are skipped.
fn parse_headers(mut bytes: &[u8]) -> Result<HashMap<String, String>, String> {
	let mut headers = HashMap::new();
	while !bytes.is_empty() {
		let name_len = *bytes.first().expect("checked above") as usize;
		bytes = &bytes[1..];
		if bytes.len() < name_len + 1 {
			return Err("truncated header name".to_string());
		}
		let name = std::str::from_utf8(&bytes[..name_len])
			.map_err(|_| "non-UTF-8 header name".to_string())?
			.to_string();
		bytes = &bytes[name_len..];
		let value_type = bytes[0];
		bytes = &bytes[1..];
		let (consumed, value) = match value_type {
			0 | 1 => (0, None), // true / false
			2 => (1, None),     // byte
			3 => (2, None),     // short
			4 => (4, None),     // int
			5 | 8 => (8, None), // long / timestamp
			9 => (16, None),    // uuid
			6 | 7 => {
				if bytes.len() < 2 {
					return Err("truncated header value length".to_string());
				}
				let len = u16::from_be_bytes([bytes[0], bytes[1]]) as usize;
				if bytes.len() < 2 + len {
					return Err("truncated header value".to_string());
				}
				let value = if value_type == 7 {
					Some(
						std::str::from_utf8(&bytes[2..2 + len])
							.map_err(|_| "non-UTF-8 header value".to_string())?
							.to_string(),
					)
				} else {
					None
				};
				(2 + len, value)
			}
			other => return Err(format!("unknown header value type {other}")),
		};
		bytes = &bytes[consumed..];
		if let Some(value) = value {
			headers.insert(name, value);
		}
	}
	Ok(headers)
}

fn crc32(data: &[u8]) -> u32 {
	let mut hasher = crc32fast::Hasher::new();
	hasher.update(data);
	hasher.finalize()
}

// ---------------------------------------------------------------------------
// Stream session — the event loop of `bedrock-converse-stream.ts`
// ---------------------------------------------------------------------------

/// Stream state of one open Bedrock content block, keyed by
/// `contentBlockIndex` and kept off the content blocks so it never reaches
/// partial messages.
struct OpenBlock {
	content_index: usize,
	arguments: Option<ToolArgsStream>,
	/// Encrypted reasoning deltas, joined into `thinkingSignature` when the
	/// block ends.
	redacted_chunks: Vec<Vec<u8>>,
}

/// Accumulates the assistant response and turns decoded event-stream frames
/// into protocol events.
struct StreamSession {
	message: AssistantMessage,
	open_blocks: HashMap<u64, OpenBlock>,
}

impl StreamSession {
	fn new(model: &Model) -> Self {
		Self {
			message: AssistantMessage {
				content: Vec::new(),
				api: model.api.as_str().to_string(),
				provider: model.provider.clone(),
				model: model.id.clone(),
				response_model: None,
				response_id: None,
				thinking_level: None,
				usage: Usage::default(),
				stop_reason: StopReason::Pending,
				error_message: None,
				raw_stop_reason: None,
				end_turn: None,
				timestamp: now_ms(),
				duration_ms: None,
			},
			open_blocks: HashMap::new(),
		}
	}

	fn into_message(self) -> AssistantMessage {
		self.message
	}

	fn process_frame(&mut self, frame: &EsFrame) -> Result<Vec<Event>, String> {
		match frame.message_type.as_str() {
			"event" => self.process_event(&frame.payload, frame.event_type.as_deref()),
			"exception" => Err(format_bedrock_error(
				frame
					.exception_type
					.as_deref()
					.unwrap_or("UnknownException"),
				&frame.payload,
			)),
			"error" => Err(frame
				.error_message
				.clone()
				.unwrap_or_else(|| "Bedrock stream error".to_string())),
			other => Err(format!("unknown event-stream message type: {other}")),
		}
	}

	fn process_event(
		&mut self,
		payload: &[u8],
		event_type: Option<&str>,
	) -> Result<Vec<Event>, String> {
		let text = String::from_utf8_lossy(payload);
		let event: Value = serde_json::from_str(text.trim()).map_err(|error| {
			format!(
				"Could not parse Bedrock event {:?} payload: {error}; data={}",
				event_type.unwrap_or("<unknown>"),
				text.trim()
			)
		})?;
		if let Some(name) = event.as_object().and_then(|object| object.keys().next()) {
			match name.as_str() {
				"messageStart" => return self.process_message_start(&event),
				"contentBlockStart" => return self.process_block_start(&event),
				"contentBlockDelta" => return self.process_block_delta(&event),
				"contentBlockStop" => return self.process_block_stop(&event),
				"messageStop" => return self.process_message_stop(&event),
				"metadata" => return Ok(self.process_metadata(&event)),
				_ => return Ok(Vec::new()),
			}
		}
		Ok(Vec::new())
	}

	fn process_message_start(&mut self, event: &Value) -> Result<Vec<Event>, String> {
		if event["messageStart"]["role"] != "assistant" {
			return Err(
				"Unexpected assistant message start but got user message start instead".to_string(),
			);
		}
		Ok(vec![Event::Start {
			partial: self.message.clone(),
		}])
	}

	fn process_block_start(&mut self, event: &Value) -> Result<Vec<Event>, String> {
		let start = &event["contentBlockStart"];
		let block_index = start["contentBlockIndex"].as_u64().unwrap_or(0);
		if let Some(tool_use) = start["start"]["toolUse"].as_object() {
			self.message
				.content
				.push(AssistantContent::ToolCall(ToolCall {
					id: tool_use
						.get("toolUseId")
						.and_then(Value::as_str)
						.unwrap_or_default()
						.to_string(),
					name: tool_use
						.get("name")
						.and_then(Value::as_str)
						.unwrap_or_default()
						.to_string(),
					arguments: json!({}),
					thought_signature: None,
					namespace: None,
					arguments_error: None,
				}));
			let content_index = self.message.content.len() - 1;
			self.open_blocks.insert(
				block_index,
				OpenBlock {
					content_index,
					arguments: Some(ToolArgsStream::new()),
					redacted_chunks: Vec::new(),
				},
			);
			return Ok(vec![Event::ToolCallStart {
				content_index,
				partial: self.message.clone(),
			}]);
		}
		Ok(Vec::new())
	}

	fn process_block_delta(&mut self, event: &Value) -> Result<Vec<Event>, String> {
		let delta_event = &event["contentBlockDelta"];
		let block_index = delta_event["contentBlockIndex"].as_u64().unwrap_or(0);
		let delta = &delta_event["delta"];
		let mut events = Vec::new();

		if let Some(text) = delta["text"].as_str() {
			// Text blocks have no start event; the first delta creates one.
			if !self.open_blocks.contains_key(&block_index) {
				self.message
					.content
					.push(AssistantContent::Text(crate::types::TextContent {
						text: String::new(),
						text_signature: None,
					}));
				let content_index = self.message.content.len() - 1;
				self.open_blocks.insert(
					block_index,
					OpenBlock {
						content_index,
						arguments: None,
						redacted_chunks: Vec::new(),
					},
				);
				events.push(Event::TextStart {
					content_index,
					partial: self.message.clone(),
				});
			}
			let open = &self.open_blocks[&block_index];
			if let Some(AssistantContent::Text(block)) =
				self.message.content.get_mut(open.content_index)
			{
				block.text.push_str(text);
				events.push(Event::TextDelta {
					content_index: open.content_index,
					delta: text.to_string(),
					partial: self.message.clone(),
				});
			}
		} else if let Some(input) = delta["toolUse"]["input"].as_str() {
			let appended = match self.open_blocks.get_mut(&block_index) {
				Some(open) if open.arguments.is_some() => {
					let stream = open.arguments.as_mut().expect("checked above");
					Some((open.content_index, stream.append(input)))
				}
				_ => None,
			};
			if let Some((content_index, arguments)) = appended {
				if let Some(AssistantContent::ToolCall(block)) =
					self.message.content.get_mut(content_index)
				{
					block.arguments = arguments;
				}
				events.push(Event::ToolCallDelta {
					content_index,
					delta: input.to_string(),
					partial: self.message.clone(),
				});
			}
		} else if delta["reasoningContent"].is_object() {
			if !self.open_blocks.contains_key(&block_index) {
				self.message
					.content
					.push(AssistantContent::Thinking(ThinkingContent {
						thinking: String::new(),
						thinking_signature: Some(String::new()),
						redacted: None,
					}));
				let content_index = self.message.content.len() - 1;
				self.open_blocks.insert(
					block_index,
					OpenBlock {
						content_index,
						arguments: None,
						redacted_chunks: Vec::new(),
					},
				);
				events.push(Event::ThinkingStart {
					content_index,
					partial: self.message.clone(),
				});
			}
			let content_index = self.open_blocks[&block_index].content_index;
			let reasoning = &delta["reasoningContent"];

			if let Some(text) = reasoning["text"].as_str()
				&& let Some(AssistantContent::Thinking(block)) =
					self.message.content.get_mut(content_index)
			{
				block.thinking.push_str(text);
				events.push(Event::ThinkingDelta {
					content_index,
					delta: text.to_string(),
					partial: self.message.clone(),
				});
			}
			// `thinkingSignature` holds either an Anthropic signature or an
			// opaque redacted payload, never both.
			if let Some(signature) = reasoning["signature"].as_str()
				&& !signature.is_empty()
				&& let Some(AssistantContent::Thinking(block)) =
					self.message.content.get_mut(content_index)
				&& block.redacted != Some(true)
			{
				let combined = format!(
					"{}{signature}",
					block.thinking_signature.clone().unwrap_or_default()
				);
				block.thinking_signature = Some(combined);
			}
			if let Some(redacted) = reasoning["redactedContent"].as_str()
				&& !redacted.is_empty()
				&& let Some(AssistantContent::Thinking(block)) =
					self.message.content.get_mut(content_index)
			{
				// Encrypted reasoning from non-Anthropic models on Bedrock
				// (e.g. OpenAI GPT-5.6). The payload is opaque, so keep it
				// verbatim in `thinkingSignature` the way the Anthropic path
				// stores redacted thinking, and replay it on the next turn.
				if block.redacted != Some(true) {
					block.redacted = Some(true);
					block.thinking_signature = Some(String::new());
					block.thinking.push_str(REDACTED_THINKING_PLACEHOLDER);
					events.push(Event::ThinkingDelta {
						content_index,
						delta: REDACTED_THINKING_PLACEHOLDER.to_string(),
						partial: self.message.clone(),
					});
				}
				if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(redacted) {
					self.open_blocks
						.get_mut(&block_index)
						.expect("inserted above")
						.redacted_chunks
						.push(bytes);
				}
			}
		}
		Ok(events)
	}

	fn process_block_stop(&mut self, event: &Value) -> Result<Vec<Event>, String> {
		let block_index = event["contentBlockStop"]["contentBlockIndex"]
			.as_u64()
			.unwrap_or(0);
		let Some(open) = self.open_blocks.remove(&block_index) else {
			return Ok(Vec::new());
		};
		let index = open.content_index;
		let mut events = Vec::new();
		match self.message.content.get_mut(index) {
			Some(AssistantContent::Text(block)) => {
				events.push(Event::TextEnd {
					content_index: index,
					content: block.text.clone(),
					partial: self.message.clone(),
				});
			}
			Some(AssistantContent::Thinking(block)) => {
				if !open.redacted_chunks.is_empty() {
					let joined = open.redacted_chunks.concat();
					block.thinking_signature =
						Some(base64::engine::general_purpose::STANDARD.encode(&joined));
				}
				events.push(Event::ThinkingEnd {
					content_index: index,
					content: block.thinking.clone(),
					partial: self.message.clone(),
				});
			}
			Some(AssistantContent::ToolCall(block)) => {
				if let Some(stream) = &open.arguments {
					let (arguments, arguments_error) = stream.finish();
					block.arguments = arguments;
					block.arguments_error = arguments_error;
				}
				events.push(Event::ToolCallEnd {
					content_index: index,
					tool_call: block.clone(),
					partial: self.message.clone(),
				});
			}
			None => {}
		}
		Ok(events)
	}

	fn process_message_stop(&mut self, event: &Value) -> Result<Vec<Event>, String> {
		let reason = event["messageStop"]["stopReason"].as_str();
		self.message.raw_stop_reason = reason.map(str::to_string);
		let (stop_reason, error_message) = map_stop_reason(reason);
		self.message.stop_reason = stop_reason;
		self.message.error_message = error_message;
		Ok(Vec::new())
	}

	fn process_metadata(&mut self, event: &Value) -> Vec<Event> {
		let usage = &event["metadata"]["usage"];
		if !usage.is_object() {
			return Vec::new();
		}
		self.message.usage.input = usage["inputTokens"].as_u64().unwrap_or(0);
		self.message.usage.output = usage["outputTokens"].as_u64().unwrap_or(0);
		self.message.usage.cache_read = usage["cacheReadInputTokens"].as_u64().unwrap_or(0);
		self.message.usage.cache_write = usage["cacheWriteInputTokens"].as_u64().unwrap_or(0);
		self.message.usage.cache_write_1h = usage["cacheDetails"].as_array().map(|details| {
			details
				.iter()
				.filter(|detail| detail["ttl"] == "1h")
				.filter_map(|detail| detail["inputTokens"].as_u64())
				.sum()
		});
		let reported = usage["totalTokens"].as_u64().unwrap_or(0);
		self.message.usage.total_tokens = if reported > 0 {
			reported
		} else {
			self.message.usage.input + self.message.usage.output
		};
		Vec::new()
	}

	/// Ends the stream state of blocks a stream settled without stopping.
	fn flush_open_blocks(&mut self) {
		for open in self.open_blocks.values() {
			if open.redacted_chunks.is_empty() {
				continue;
			}
			if let Some(AssistantContent::Thinking(block)) =
				self.message.content.get_mut(open.content_index)
			{
				let joined = open.redacted_chunks.concat();
				block.thinking_signature =
					Some(base64::engine::general_purpose::STANDARD.encode(&joined));
			}
		}
		self.open_blocks.clear();
	}
}

/// Port of `mapStopReason`; the error variant carries the message text.
fn map_stop_reason(reason: Option<&str>) -> (StopReason, Option<String>) {
	match reason {
		Some("end_turn") | Some("stop_sequence") => (StopReason::Stop, None),
		Some("max_tokens") | Some("model_context_window_exceeded") => (StopReason::Length, None),
		Some("tool_use") => (StopReason::ToolUse, None),
		Some(other) => (
			StopReason::Error,
			Some(format!("Provider stopped with: {other}")),
		),
		None => (StopReason::Error, None),
	}
}

fn format_bedrock_error(exception_name: &str, payload: &[u8]) -> String {
	let payload_text = String::from_utf8_lossy(payload).trim().to_string();
	let message = serde_json::from_str::<Value>(&payload_text)
		.ok()
		.and_then(|value| value["message"].as_str().map(str::to_string))
		.unwrap_or(payload_text);
	let prefix = BEDROCK_ERROR_PREFIXES
		.iter()
		.find(|(name, _)| *name == exception_name)
		.map(|(_, prefix)| *prefix)
		.unwrap_or(exception_name);
	with_data_retention_hint(format!("{prefix}: {message}"))
}

/// Point users at the AWS docs when a model rejects the configured data
/// retention mode (upstream `BEDROCK_DATA_RETENTION_DOCS_URL`).
fn with_data_retention_hint(message: String) -> String {
	if message.to_lowercase().contains("data retention mode") {
		format!(
			"{message} See {BEDROCK_DATA_RETENTION_DOCS_URL} for supported data retention modes."
		)
	} else {
		message
	}
}

// ---------------------------------------------------------------------------
// Request body
// ---------------------------------------------------------------------------

/// Cache retention preference: explicit setting > `PI_CACHE_RETENTION` env >
/// short (port of `resolveCacheRetention`).
fn resolve_cache_retention() -> &'static str {
	if std::env::var("PI_CACHE_RETENTION").ok().as_deref() == Some("long") {
		"long"
	} else {
		"short"
	}
}

/// `POST /model/{id}/converse-stream` body — port of the `commandInput` in
/// the upstream `stream()`.
fn request_body(model: &Model, context: &TranscriptContext, options: &StreamOptions) -> Value {
	let system_prompt = system_prompt(context);
	let tools = current_tools(context);
	let cache_retention = resolve_cache_retention();
	let caching = cache_retention != "none" && supports_prompt_caching(model);

	// The TS adapter folds mid-conversation system messages into the leading
	// prompt first, so the message conversion skips every system message.
	let mut messages: Vec<Value> = Vec::new();
	let index_of_first_non_system = context
		.messages
		.iter()
		.position(|message| !matches!(message, Message::System(_)))
		.unwrap_or(context.messages.len());
	convert_messages(
		&context.messages[index_of_first_non_system..],
		model,
		caching,
		&mut messages,
	);

	let mut body = json!({
		"modelId": model.id,
		"messages": messages,
	});
	if !system_prompt.is_empty() {
		let mut system = vec![json!({"text": system_prompt})];
		if caching {
			system.push(cache_point(cache_retention));
		}
		body["system"] = json!(system);
	}
	let mut inference_config = serde_json::Map::new();
	// For Claude the model catalog cap applies when the caller left it unset.
	let max_tokens = match options.max_tokens {
		Some(tokens) => Some(tokens),
		None => {
			(is_anthropic_claude_model(model) && model.max_tokens > 0).then_some(model.max_tokens)
		}
	};
	if let Some(max_tokens) = max_tokens {
		inference_config.insert("maxTokens".to_string(), json!(max_tokens));
	}
	if let Some(temperature) = options.temperature {
		inference_config.insert("temperature".to_string(), json!(temperature));
	}
	if !inference_config.is_empty() {
		body["inferenceConfig"] = Value::Object(inference_config);
	}
	if !tools.is_empty() {
		// toolChoice is a deferred StreamOptions gap; strict-mode schemas
		// require `compat.supportsStrictMode`, which is not modeled.
		let specs: Vec<Value> = tools
			.iter()
			.map(|tool| {
				json!({"toolSpec": {
					"name": tool.name,
					"description": tool.description,
					"inputSchema": {"json": tool.parameters},
				}})
			})
			.collect();
		body["toolConfig"] = json!({"tools": specs});
	}
	if let Some(fields) = build_additional_model_request_fields(model, options) {
		body["additionalModelRequestFields"] = fields;
	}
	body
}

fn cache_point(retention: &str) -> Value {
	if retention == "long" {
		json!({"cachePoint": {"type": "default", "ttl": "1h"}})
	} else {
		json!({"cachePoint": {"type": "default"}})
	}
}

/// Collapsed system prompt: every system message's text joined, section
/// overrides patched by name (`collapseSystemMessages` + `getSystemMessageText`).
fn system_prompt(context: &TranscriptContext) -> String {
	let mut content: Vec<String> = Vec::new();
	let mut sections: std::collections::BTreeMap<String, String> =
		std::collections::BTreeMap::new();
	for message in &context.messages {
		let Message::System(m) = message else {
			continue;
		};
		let text = match &m.content {
			crate::types::SystemContent::Text(s) => s.clone(),
			crate::types::SystemContent::Blocks(b) => b
				.iter()
				.map(|x| x.text.as_str())
				.collect::<Vec<_>>()
				.join("\n"),
		};
		if !text.is_empty() {
			content.push(text);
		}
		for (name, value) in m.sections.iter().flatten() {
			match value {
				Some(text) => {
					sections.insert(name.clone(), text.clone());
				}
				None => {
					sections.remove(name);
				}
			}
		}
	}
	content.extend(sections.into_values());
	content
		.into_iter()
		.filter(|part| !part.is_empty())
		.collect::<Vec<_>>()
		.join("\n\n")
}

/// Current tool set after replaying every `tools_added`/`tools_removed` delta
/// in order (port of `getCurrentTools`), preserving declaration order.
fn current_tools(context: &TranscriptContext) -> Vec<crate::types::Tool> {
	let mut tools: Vec<(String, crate::types::Tool)> = Vec::new();
	for message in &context.messages {
		let Message::System(m) = message else {
			continue;
		};
		for removed in m.tools_removed.iter().flatten() {
			tools.retain(|(name, _)| name != &removed.name);
		}
		for added in m.tools_added.iter().flatten() {
			match tools.iter_mut().find(|(name, _)| name == &added.name) {
				Some(slot) => slot.1 = added.clone(),
				None => tools.push((added.name.clone(), added.clone())),
			}
		}
	}
	tools.into_iter().map(|(_, tool)| tool).collect()
}

/// Tool result content blocks — blank text collapses away, images become
/// image blocks, an all-empty result degrades to `<empty>`.
fn convert_tool_result_content(content: &[TextOrImage]) -> Vec<Value> {
	let mut result: Vec<Value> = Vec::new();
	for block in content {
		match block {
			TextOrImage::Image(image) => result.push(image_block(image)),
			TextOrImage::Text(text) => {
				if let Some(block) = non_blank_text_block(&text.text) {
					result.push(block);
				}
			}
		}
	}
	if result.is_empty() {
		result.push(json!({"text": EMPTY_TEXT_PLACEHOLDER}));
	}
	result
}

fn non_blank_text_block(text: &str) -> Option<Value> {
	(!text.trim().is_empty()).then(|| json!({"text": text}))
}

fn image_block(image: &ImageContent) -> Value {
	let format = match image.mime_type.as_str() {
		"image/jpeg" | "image/jpg" => "jpeg",
		"image/png" => "png",
		"image/gif" => "gif",
		"image/webp" => "webp",
		other => other, // Bedrock rejects unknown formats server-side; upstream throws client-side
	};
	json!({"image": {"format": format, "source": {"bytes": image.data}}})
}

/// Recursively drops empty-string object keys (`sanitizeBedrockDocument`).
fn sanitize_bedrock_document(value: &Value) -> Value {
	match value {
		Value::Array(items) => Value::Array(items.iter().map(sanitize_bedrock_document).collect()),
		Value::Object(object) => Value::Object(
			object
				.iter()
				.filter(|(key, _)| !key.is_empty())
				.map(|(key, nested)| (key.clone(), sanitize_bedrock_document(nested)))
				.collect(),
		),
		other => other.clone(),
	}
}

/// Whether images from error tool results must ride behind all tool results of
/// the turn (Claude rejects error tool results containing anything but text).
/// Returns the in-place content (images replaced by a placeholder) and the
/// trailing caption + image blocks.
fn split_error_tool_result_images(
	message: &crate::types::ToolResultMessage,
) -> Option<(Vec<Value>, Vec<Value>)> {
	if !message.is_error {
		return None;
	}
	let images: Vec<&ImageContent> = message
		.content
		.iter()
		.filter_map(|block| match block {
			TextOrImage::Image(image) => Some(image),
			TextOrImage::Text(_) => None,
		})
		.collect();
	if images.is_empty() {
		return None;
	}
	let mut content: Vec<Value> = Vec::new();
	let mut previous_was_placeholder = false;
	for block in &message.content {
		match block {
			TextOrImage::Image(_) => {
				if !previous_was_placeholder {
					content.push(json!({"text": ERROR_TOOL_IMAGE_PLACEHOLDER}));
				}
				previous_was_placeholder = true;
			}
			TextOrImage::Text(text) => {
				content.push(json!({"text": text.text}));
				previous_was_placeholder = text.text == ERROR_TOOL_IMAGE_PLACEHOLDER;
			}
		}
	}
	let mut tail = vec![json!({"text": format!(
		"Images from the error result of tool call {} ({}):",
		message.tool_call_id, message.tool_name
	)})];
	tail.extend(images.iter().map(|image| image_block(image)));
	Some((content, tail))
}

/// `convertMessages` for the Converse wire — skips system messages (already
/// folded into the prompt), merges consecutive tool results into one user
/// message, and adds the cache point to the final user message.
fn convert_messages(messages: &[Message], model: &Model, caching: bool, result: &mut Vec<Value>) {
	let mut index = 0;
	while index < messages.len() {
		match &messages[index] {
			Message::System(_) => {
				// Collapsed into the top-level system prompt.
				index += 1;
			}
			Message::User(m) => {
				let mut content: Vec<Value> = Vec::new();
				match &m.content {
					crate::types::UserContent::Text(text) => {
						content.push(required_text_block(text));
					}
					crate::types::UserContent::Blocks(blocks) => {
						for block in blocks {
							match block {
								TextOrImage::Text(text) => {
									if let Some(block) = non_blank_text_block(&text.text) {
										content.push(block);
									}
								}
								TextOrImage::Image(image) => content.push(image_block(image)),
							}
						}
						if content.is_empty() {
							content.push(json!({"text": EMPTY_TEXT_PLACEHOLDER}));
						}
					}
				}
				result.push(json!({"role": "user", "content": content}));
				index += 1;
			}
			Message::Assistant(m) => {
				// Skip assistant messages with empty content (e.g. from aborted
				// requests); Bedrock rejects empty content arrays.
				if m.content.is_empty() {
					index += 1;
					continue;
				}
				let mut blocks: Vec<Value> = Vec::new();
				for block in &m.content {
					match block {
						AssistantContent::Text(text) => {
							if let Some(block) = non_blank_text_block(&text.text) {
								blocks.push(block);
							}
						}
						AssistantContent::ToolCall(call) => blocks.push(json!({
							"toolUse": {
								"toolUseId": call.id,
								"name": call.name,
								"input": sanitize_bedrock_document(&call.arguments),
							}
						})),
						AssistantContent::Thinking(thinking) => {
							push_thinking_block(thinking, model, &mut blocks);
						}
					}
				}
				// Skip if all content blocks were filtered out.
				if !blocks.is_empty() {
					result.push(json!({"role": "assistant", "content": blocks}));
				}
				index += 1;
			}
			Message::ToolResult(_) => {
				// All tool results of the turn share one user message.
				let mut tool_results: Vec<Value> = Vec::new();
				let mut error_images: Vec<Value> = Vec::new();
				while let Some(Message::ToolResult(tool_msg)) = messages.get(index) {
					let split = if is_anthropic_claude_model(model) {
						split_error_tool_result_images(tool_msg)
					} else {
						None
					};
					let (content, tail) = match split {
						Some((content, tail)) => (content, Some(tail)),
						None => (convert_tool_result_content(&tool_msg.content), None),
					};
					tool_results.push(json!({
						"toolResult": {
							"toolUseId": tool_msg.tool_call_id,
							"content": content,
							"status": if tool_msg.is_error { "error" } else { "success" },
						}
					}));
					if let Some(mut tail) = tail {
						error_images.append(&mut tail);
					}
					index += 1;
				}
				let mut content = tool_results;
				content.extend(error_images);
				result.push(json!({"role": "user", "content": content}));
			}
		}
	}

	// Cache point on the last user message so the conversation prefix is
	// cacheable for supported Claude models.
	if caching
		&& let Some(last) = result.last_mut()
		&& last["role"] == "user"
		&& let Some(content) = last["content"].as_array_mut()
	{
		content.push(cache_point(resolve_cache_retention()));
	}
}

/// Replayed thinking blocks — port of the assistant `thinking` branch.
/// Encrypted reasoning rides verbatim as `redactedContent`; signed thinking
/// only replays with its signature on Claude models.
fn push_thinking_block(thinking: &ThinkingContent, model: &Model, blocks: &mut Vec<Value>) {
	if thinking.redacted == Some(true) {
		// A hand-edited or externally produced session can hold a signature
		// that is not base64; drop that block instead of failing the request.
		let signature = thinking.thinking_signature.as_deref().unwrap_or_default();
		if !signature.is_empty()
			&& base64::engine::general_purpose::STANDARD
				.decode(signature)
				.is_ok()
		{
			blocks.push(json!({"reasoningContent": {"redactedContent": signature}}));
		}
		return;
	}
	if thinking.thinking.trim().is_empty() {
		return;
	}
	if supports_thinking_signature(model) {
		let signature = thinking.thinking_signature.as_deref().unwrap_or("").trim();
		if signature.is_empty() {
			// Missing signature (e.g. aborted stream): replay as plain text,
			// matching the Anthropic path.
			blocks.push(json!({"text": thinking.thinking}));
		} else {
			blocks.push(json!({"reasoningContent": {"reasoningText": {
				"text": thinking.thinking,
				"signature": signature,
			}}}));
		}
	} else {
		blocks.push(json!({"reasoningContent": {"reasoningText": {"text": thinking.thinking}}}));
	}
}

fn required_text_block(text: &str) -> Value {
	non_blank_text_block(text).unwrap_or_else(|| json!({"text": EMPTY_TEXT_PLACEHOLDER}))
}

// ---------------------------------------------------------------------------
// Model predicates and reasoning fields
// ---------------------------------------------------------------------------

/// Lowercase match candidates for a model id plus its display name, with
/// separators normalized to dashes (port of `getModelMatchCandidates`).
fn model_match_candidates(model: &Model) -> Vec<String> {
	let mut candidates = Vec::new();
	for value in [&model.id, &model.name] {
		let lower = value.to_lowercase();
		candidates.push(lower.clone());
		candidates.push(
			lower
				.split([' ', '_', '.', ':'])
				.filter(|part| !part.is_empty())
				.collect::<Vec<_>>()
				.join("-"),
		);
	}
	candidates
}

fn candidates_include(candidates: &[String], needle: &str) -> bool {
	candidates
		.iter()
		.any(|candidate| candidate.contains(needle))
}

fn is_anthropic_claude_model(model: &Model) -> bool {
	let id = model.id.to_lowercase();
	let name = model.name.to_lowercase();
	id.contains("anthropic.claude")
		|| id.contains("anthropic/claude")
		|| name.contains("anthropic.claude")
		|| name.contains("anthropic/claude")
		|| name.contains("claude")
}

/// Prompt caching: Claude 3.5 Haiku / 3.7 Sonnet / 4.x / 5 families, or the
/// `AWS_BEDROCK_FORCE_CACHE=1` escape hatch for application inference profiles.
fn supports_prompt_caching(model: &Model) -> bool {
	let candidates = model_match_candidates(model);
	if !candidates_include(&candidates, "claude") {
		if std::env::var("AWS_BEDROCK_FORCE_CACHE").ok().as_deref() == Some("1") {
			return true;
		}
		return false;
	}
	candidates_include(&candidates, "fable-5")
		|| candidates_include(&candidates, "opus-5")
		|| candidates_include(&candidates, "sonnet-5")
		|| candidates_include(&candidates, "haiku-5")
		|| candidates_include(&candidates, "-4-")
		|| candidates_include(&candidates, "claude-3-7-sonnet")
		|| candidates_include(&candidates, "claude-3-5-haiku")
}

/// Adaptive thinking (Opus 4.6+, Sonnet 4.6, Claude 5).
fn supports_adaptive_thinking(model: &Model) -> bool {
	let candidates = model_match_candidates(model);
	candidates_include(&candidates, "opus-4-6")
		|| candidates_include(&candidates, "opus-4-7")
		|| candidates_include(&candidates, "opus-4-8")
		|| candidates_include(&candidates, "opus-5")
		|| candidates_include(&candidates, "sonnet-4-6")
		|| candidates_include(&candidates, "sonnet-5")
		|| candidates_include(&candidates, "haiku-5")
		|| candidates_include(&candidates, "fable-5")
}

fn supports_native_xhigh_effort(model: &Model) -> bool {
	let candidates = model_match_candidates(model);
	candidates_include(&candidates, "opus-4-7")
		|| candidates_include(&candidates, "opus-4-8")
		|| candidates_include(&candidates, "opus-5")
		|| candidates_include(&candidates, "sonnet-5")
		|| candidates_include(&candidates, "haiku-5")
		|| candidates_include(&candidates, "fable-5")
}

/// Opus 4.6 and Sonnet 4.6 reject `thinking.block_binding`.
fn supports_thinking_block_binding(model: &Model) -> bool {
	let candidates = model_match_candidates(model);
	candidates_include(&candidates, "opus-4-7")
		|| candidates_include(&candidates, "opus-4-8")
		|| candidates_include(&candidates, "opus-5")
		|| candidates_include(&candidates, "sonnet-5")
		|| candidates_include(&candidates, "haiku-5")
		|| candidates_include(&candidates, "fable-5")
}

/// Only Claude models accept `reasoningText.signature`.
fn supports_thinking_signature(model: &Model) -> bool {
	is_anthropic_claude_model(model)
}

fn is_govcloud_bedrock_target(model: &Model) -> bool {
	let region = std::env::var("AWS_REGION")
		.ok()
		.or_else(|| std::env::var("AWS_DEFAULT_REGION").ok())
		.unwrap_or_default();
	if region.to_lowercase().starts_with("us-gov-") {
		return true;
	}
	let model_id = model.id.to_lowercase();
	model_id.starts_with("us-gov.") || model_id.starts_with("arn:aws-us-gov:")
}

/// `additionalModelRequestFields` — port of `buildAdditionalModelRequestFields`
/// for Claude (adaptive or budget thinking), gpt-oss and OpenAI GPT models.
fn build_additional_model_request_fields(model: &Model, options: &StreamOptions) -> Option<Value> {
	let reasoning = options.reasoning?;
	if reasoning == ThinkingLevel::Off || !model.reasoning {
		return None;
	}

	if is_anthropic_claude_model(model) {
		// GovCloud Bedrock currently rejects the Claude thinking.display field
		// and the block binding; omit both there.
		let govcloud = is_govcloud_bedrock_target(model);
		let display = (!govcloud).then_some("summarized");
		let use_block_binding = !govcloud && supports_thinking_block_binding(model);
		let effort = map_thinking_level_to_effort(model, reasoning);
		let mut result = if supports_adaptive_thinking(model) {
			let mut thinking = json!({"type": "adaptive"});
			if let Some(display) = display {
				thinking["display"] = json!(display);
			}
			if use_block_binding {
				thinking["block_binding"] = json!({"prefix_mismatch_behavior": "drop_block"});
			}
			let mut result = json!({"thinking": thinking, "output_config": {"effort": effort}});
			if use_block_binding {
				result["anthropic_beta"] = json!([THINKING_BINDING_CONTROLS_BETA]);
			}
			result
		} else {
			let default_budgets: [(ThinkingLevel, u64); 6] = [
				(ThinkingLevel::Minimal, 1024),
				(ThinkingLevel::Low, 2048),
				(ThinkingLevel::Medium, 8192),
				(ThinkingLevel::High, 16_384),
				// Budget-based Claude clamps extended levels to high.
				(ThinkingLevel::Xhigh, 16_384),
				(ThinkingLevel::Max, 16_384),
			];
			let budget = default_budgets
				.iter()
				.find(|(level, _)| *level == reasoning)
				.map(|(_, budget)| *budget)
				.unwrap_or(16_384);
			let mut thinking = json!({"type": "enabled", "budget_tokens": budget});
			if let Some(display) = display {
				thinking["display"] = json!(display);
			}
			json!({"thinking": thinking})
		};
		if !supports_adaptive_thinking(model) {
			result["anthropic_beta"] = json!(["interleaved-thinking-2025-05-14"]);
		}
		return Some(result);
	}

	let candidates = model_match_candidates(model);
	if candidates_include(&candidates, "gpt-oss") {
		// gpt-oss takes a flat `reasoning_effort` and only accepts low,
		// medium and high.
		let effort = match reasoning {
			ThinkingLevel::Minimal | ThinkingLevel::Low => "low",
			ThinkingLevel::Medium => "medium",
			_ => "high",
		};
		return Some(json!({"reasoning_effort": effort}));
	}
	if candidates_include(&candidates, "gpt-") {
		// OpenAI GPT models take a nested `reasoning.effort` and reject
		// `minimal`.
		let effort = match reasoning {
			ThinkingLevel::Minimal | ThinkingLevel::Low => "low",
			ThinkingLevel::Medium => "medium",
			ThinkingLevel::High => "high",
			ThinkingLevel::Xhigh => "xhigh",
			_ => "max",
		};
		return Some(json!({"reasoning": {"effort": effort}}));
	}
	None
}

/// Effort mapping for Claude adaptive thinking; native `xhigh` only on
/// Claude 5 / Opus 4.7+.
fn map_thinking_level_to_effort(model: &Model, level: ThinkingLevel) -> &'static str {
	if level == ThinkingLevel::Xhigh && supports_native_xhigh_effort(model) {
		return "xhigh";
	}
	match level {
		ThinkingLevel::Minimal | ThinkingLevel::Low => "low",
		ThinkingLevel::Medium => "medium",
		ThinkingLevel::High | ThinkingLevel::Xhigh | ThinkingLevel::Max => "high",
		ThinkingLevel::Off => "high",
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::model::Api;
	use crate::types::{SystemMessage, Tool, ToolResultMessage, UserMessage};

	fn bedrock_model() -> Model {
		let mut model = Model::new(
			"anthropic.claude-sonnet-4-6-v1:0",
			"Sonnet 4.6 (Bedrock)",
			Api::BEDROCK_CONVERSE_STREAM,
			"amazon-bedrock",
			"",
		);
		model.reasoning = true;
		model.context_window = 200_000;
		model.max_tokens = 64_000;
		model
	}

	fn plain_model() -> Model {
		let mut model = bedrock_model();
		model.id = "meta.llama3-70b-instruct-v1:0".to_string();
		model.name = "Llama 3 70B".to_string();
		model.reasoning = false;
		model
	}

	fn system_with_tools() -> SystemMessage {
		let mut system = SystemMessage::prompt("help");
		system.tools_added = Some(vec![Tool::new("ls", "list", json!({"type": "object"}))]);
		system
	}

	// -- request body -------------------------------------------------------

	#[test]
	fn request_body_carries_system_tools_and_cache_point() {
		let model = bedrock_model();
		let ctx = TranscriptContext {
			messages: vec![Message::System(system_with_tools()), Message::user("list")],
		};
		let body = request_body(&model, &ctx, &StreamOptions::default());
		assert_eq!(body["modelId"], "anthropic.claude-sonnet-4-6-v1:0");
		assert_eq!(body["messages"][0]["role"], "user");
		assert_eq!(body["messages"][0]["content"][0]["text"], "list");
		assert_eq!(body["system"][0]["text"], "help");
		assert_eq!(body["system"][1]["cachePoint"]["type"], "default");
		assert_eq!(body["toolConfig"]["tools"][0]["toolSpec"]["name"], "ls");
		assert_eq!(body["inferenceConfig"]["maxTokens"], 64_000);
		assert_eq!(
			body["messages"][0]["content"][1]["cachePoint"]["type"],
			"default"
		);
	}

	#[test]
	fn request_body_omits_max_tokens_for_non_claude_models() {
		let model = plain_model();
		let ctx = TranscriptContext {
			messages: vec![Message::user("hi")],
		};
		let body = request_body(&model, &ctx, &StreamOptions::default());
		assert!(body["inferenceConfig"]["maxTokens"].is_null());
		// Llama gets no cache point and no reasoning fields.
		assert!(body["system"][1].is_null());
		assert!(body["additionalModelRequestFields"].is_null());
	}

	#[test]
	fn empty_user_text_degrades_to_placeholder() {
		let model = plain_model();
		let ctx = TranscriptContext {
			messages: vec![Message::User(UserMessage::raw("   "))],
		};
		let body = request_body(&model, &ctx, &StreamOptions::default());
		assert_eq!(body["messages"][0]["content"][0]["text"], "<empty>");
	}

	#[test]
	fn assistant_thinking_replays_with_signature_or_as_text() {
		let model = bedrock_model();
		let thinking_signed = AssistantContent::Thinking(ThinkingContent {
			thinking: "pondering".to_string(),
			thinking_signature: Some("sig==".to_string()),
			redacted: None,
		});
		let thinking_unsigned = AssistantContent::Thinking(ThinkingContent {
			thinking: "unsealed".to_string(),
			thinking_signature: Some(String::new()),
			redacted: None,
		});
		let redacted = AssistantContent::Thinking(ThinkingContent {
			thinking: REDACTED_THINKING_PLACEHOLDER.to_string(),
			thinking_signature: Some("aGVsbG8=".to_string()),
			redacted: Some(true),
		});
		let ctx = TranscriptContext {
			messages: vec![
				Message::user("hi"),
				Message::Assistant(AssistantMessage {
					content: vec![thinking_signed, thinking_unsigned, redacted],
					api: model.api.as_str().to_string(),
					provider: model.provider.clone(),
					model: model.id.clone(),
					response_model: None,
					response_id: None,
					thinking_level: None,
					usage: Usage::default(),
					stop_reason: StopReason::Stop,
					error_message: None,
					raw_stop_reason: None,
					end_turn: None,
					timestamp: now_ms(),
					duration_ms: None,
				}),
			],
		};
		let body = request_body(&model, &ctx, &StreamOptions::default());
		let content = &body["messages"][1]["content"];
		assert_eq!(
			content[0]["reasoningContent"]["reasoningText"]["signature"],
			"sig=="
		);
		// Missing signature falls back to plain text.
		assert_eq!(content[1]["text"], "unsealed");
		// Redacted payload rides verbatim as base64.
		assert_eq!(
			content[2]["reasoningContent"]["redactedContent"],
			"aGVsbG8="
		);
	}

	#[test]
	fn error_tool_result_images_move_behind_the_turn_for_claude() {
		let model = bedrock_model();
		let mut result = ToolResultMessage::text_result("call_1", "bash", "boom", true);
		result.content.push(TextOrImage::Image(ImageContent {
			data: "aGk=".to_string(),
			mime_type: "image/png".to_string(),
		}));
		let ctx = TranscriptContext {
			messages: vec![Message::user("hi"), Message::ToolResult(result)],
		};
		let body = request_body(&model, &ctx, &StreamOptions::default());
		let content = &body["messages"][1]["content"];
		assert_eq!(content[0]["toolResult"]["status"], "error");
		// The image inside the error result is replaced by a placeholder.
		assert_eq!(content[0]["toolResult"]["content"][0]["text"], "boom");
		assert_eq!(
			content[0]["toolResult"]["content"][1]["text"],
			ERROR_TOOL_IMAGE_PLACEHOLDER
		);
		// Caption and image trail all tool results of the turn.
		assert!(
			content[1]["text"]
				.as_str()
				.unwrap()
				.starts_with("Images from the error result of tool call call_1")
		);
		assert_eq!(content[2]["image"]["format"], "png");
	}

	#[test]
	fn consecutive_tool_results_merge_into_one_user_message() {
		let model = plain_model();
		let ctx = TranscriptContext {
			messages: vec![
				Message::user("hi"),
				Message::ToolResult(ToolResultMessage::text_result("c1", "ls", "a", false)),
				Message::ToolResult(ToolResultMessage::text_result("c2", "ls", "b", false)),
			],
		};
		let body = request_body(&model, &ctx, &StreamOptions::default());
		let messages = body["messages"].as_array().unwrap();
		assert_eq!(messages.len(), 2);
		assert_eq!(messages[1]["content"][0]["toolResult"]["toolUseId"], "c1");
		assert_eq!(messages[1]["content"][1]["toolResult"]["toolUseId"], "c2");
	}

	// -- reasoning fields ---------------------------------------------------

	#[test]
	fn claude_budget_thinking_maps_to_budget_tokens_and_beta() {
		// Sonnet 4.5 predates adaptive thinking: budget path with the
		// interleaved-thinking beta. (4.6+ uses the adaptive path instead.)
		let mut model = bedrock_model();
		model.id = "anthropic.claude-sonnet-4-5-20250929-v1:0".to_string();
		model.name = "Sonnet 4.5 (Bedrock)".to_string();
		let options = StreamOptions {
			reasoning: Some(ThinkingLevel::Medium),
			..Default::default()
		};
		let fields = build_additional_model_request_fields(&model, &options).unwrap();
		assert_eq!(fields["thinking"]["type"], "enabled");
		assert_eq!(fields["thinking"]["budget_tokens"], 8192);
		assert_eq!(fields["thinking"]["display"], "summarized");
		assert_eq!(
			fields["anthropic_beta"],
			json!(["interleaved-thinking-2025-05-14"])
		);
	}

	#[test]
	fn claude_adaptive_thinking_carries_block_binding_for_newer_models() {
		let mut model = bedrock_model();
		model.id = "us.anthropic.claude-opus-4-7-v1:0".to_string();
		let options = StreamOptions {
			reasoning: Some(ThinkingLevel::High),
			..Default::default()
		};
		let fields = build_additional_model_request_fields(&model, &options).unwrap();
		assert_eq!(fields["thinking"]["type"], "adaptive");
		assert_eq!(
			fields["thinking"]["block_binding"]["prefix_mismatch_behavior"],
			"drop_block"
		);
		assert_eq!(fields["output_config"]["effort"], "high");
		assert_eq!(
			fields["anthropic_beta"],
			json!([THINKING_BINDING_CONTROLS_BETA])
		);
	}

	#[test]
	fn gpt_models_get_nested_effort_and_oss_gets_flat() {
		let mut model = plain_model();
		model.id = "openai.gpt-5.6".to_string();
		model.reasoning = true;
		let options = StreamOptions {
			reasoning: Some(ThinkingLevel::Minimal),
			..Default::default()
		};
		let fields = build_additional_model_request_fields(&model, &options).unwrap();
		assert_eq!(fields["reasoning"]["effort"], "low");

		let mut oss = plain_model();
		oss.id = "openai.gpt-oss-120b".to_string();
		oss.reasoning = true;
		let options = StreamOptions {
			reasoning: Some(ThinkingLevel::Max),
			..Default::default()
		};
		let fields = build_additional_model_request_fields(&oss, &options).unwrap();
		assert_eq!(fields["reasoning_effort"], "high");
	}

	#[test]
	fn off_and_non_reasoning_models_send_no_fields() {
		let model = bedrock_model();
		let options = StreamOptions {
			reasoning: Some(ThinkingLevel::Off),
			..Default::default()
		};
		assert!(build_additional_model_request_fields(&model, &options).is_none());
		assert!(build_additional_model_request_fields(&model, &StreamOptions::default()).is_none());
	}

	// -- stop reasons -------------------------------------------------------

	#[test]
	fn stop_reasons_map_like_upstream() {
		let (reason, message) = map_stop_reason(Some("end_turn"));
		assert_eq!(reason, StopReason::Stop);
		assert!(message.is_none());
		let (reason, _) = map_stop_reason(Some("stop_sequence"));
		assert_eq!(reason, StopReason::Stop);
		let (reason, _) = map_stop_reason(Some("max_tokens"));
		assert_eq!(reason, StopReason::Length);
		let (reason, _) = map_stop_reason(Some("model_context_window_exceeded"));
		assert_eq!(reason, StopReason::Length);
		let (reason, _) = map_stop_reason(Some("tool_use"));
		assert_eq!(reason, StopReason::ToolUse);
		let (reason, message) = map_stop_reason(Some("guardrail_intervened"));
		assert_eq!(reason, StopReason::Error);
		assert_eq!(
			message.unwrap(),
			"Provider stopped with: guardrail_intervened"
		);
		let (reason, message) = map_stop_reason(None);
		assert_eq!(reason, StopReason::Error);
		assert!(message.is_none());
	}

	// -- auth / signing -----------------------------------------------------

	#[test]
	fn sigv4_reference_vector() {
		// AWS SigV4 documentation example (iam, 2015-08-30). The canonical
		// request, string-to-sign and final signature are pinned upstream.
		let signer = SigV4 {
			access_key: "AKIDEXAMPLE".to_string(),
			secret_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".to_string(),
			session_token: None,
			region: "us-east-1".to_string(),
			service: "iam".to_string(),
		};
		let datestamp = "20150830";
		let amz_date = "20150830T123600Z";
		let canonical_query = "Action=ListUsers&Version=2010-05-08";
		// The documentation example carries a content-type header as well.
		let canonical_request = format!(
			"GET\n/\n{canonical_query}\ncontent-type:application/x-www-form-urlencoded; charset=utf-8\nhost:iam.amazonaws.com\nx-amz-date:{amz_date}\n\ncontent-type;host;x-amz-date\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
		);
		let scope = format!("{datestamp}/us-east-1/iam/aws4_request");
		let string_to_sign = format!(
			"AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
			hex_sha256(canonical_request.as_bytes())
		);
		let k_date = hmac_sha256(
			format!("AWS4{}", signer.secret_key).as_bytes(),
			datestamp.as_bytes(),
		);
		let k_region = hmac_sha256(&k_date, b"us-east-1");
		let k_service = hmac_sha256(&k_region, b"iam");
		let k_signing = hmac_sha256(&k_service, b"aws4_request");
		assert_eq!(
			hex(&hmac_sha256(&k_signing, string_to_sign.as_bytes())),
			"5d672d79c15b13162d9279b0855cfba6789a8edb4c82c400e06b5924a6f2b5d7"
		);
	}

	#[test]
	fn sign_request_headers_cover_session_token() {
		let signer = SigV4 {
			access_key: "AKID".to_string(),
			secret_key: "secret".to_string(),
			session_token: Some("TOKEN".to_string()),
			region: "us-west-2".to_string(),
			service: "bedrock".to_string(),
		};
		let url = Url::parse(
			"https://bedrock-runtime.us-west-2.amazonaws.com/model/m.v1%3A0/converse-stream",
		)
		.unwrap();
		let headers = signer.sign_request(
			"POST",
			url.path(),
			b"{}",
			"bedrock-runtime.us-west-2.amazonaws.com",
			"20260101T000000Z",
		);
		let names: Vec<&str> = headers.iter().map(|(name, _)| name.as_str()).collect();
		assert_eq!(
			names,
			["x-amz-date", "authorization", "x-amz-security-token"]
		);
		let authorization = &headers[1].1;
		assert!(authorization.starts_with(
			"AWS4-HMAC-SHA256 Credential=AKID/20260101/us-west-2/bedrock/aws4_request"
		));
		// The signed headers list appears literally (semicolon separators).
		assert!(authorization.contains("SignedHeaders=host;x-amz-date;x-amz-security-token"));
	}

	#[test]
	fn path_encoding_escapes_colon_and_slash() {
		assert_eq!(
			encode_path_segment("anthropic.claude-sonnet-4-6-v1:0"),
			"anthropic.claude-sonnet-4-6-v1%3A0"
		);
		assert_eq!(
			encode_path_segment("arn:aws:bedrock:us-east-1:123:application-inference-profile/abc"),
			"arn%3Aaws%3Abedrock%3Aus-east-1%3A123%3Aapplication-inference-profile%2Fabc"
		);
	}

	#[test]
	fn amz_dates_format_utc() {
		assert_eq!(
			amz_dates(0),
			("19700101T000000Z".to_string(), "19700101".to_string())
		);
		// 2026-11-04T00:00:00Z is 1793750400 seconds after the epoch.
		assert_eq!(
			amz_dates(1_793_750_400_000),
			("20261104T000000Z".to_string(), "20261104".to_string())
		);
		// Leap-day arithmetic: 2024-02-29 is 1709164800.
		assert_eq!(
			amz_dates(1_709_164_800_000),
			("20240229T000000Z".to_string(), "20240229".to_string())
		);
	}

	#[test]
	fn region_resolution_follows_upstream_order() {
		let mut model = bedrock_model();
		// ARN-embedded region wins over everything.
		model.id = "arn:aws:bedrock:eu-central-1:123:application-inference-profile/abc".to_string();
		assert_eq!(arn_region(&model.id).as_deref(), Some("eu-central-1"));
		// Standard endpoint host region.
		model.id = "anthropic.claude-sonnet-4-6-v1:0".to_string();
		model.base_url = "https://bedrock-runtime-fips.ap-northeast-1.amazonaws.com".to_string();
		assert_eq!(
			standard_endpoint_region(&model.base_url).as_deref(),
			Some("ap-northeast-1")
		);
		// Non-standard hosts carry no region.
		model.base_url = "https://internal-proxy.local:8443/v1".to_string();
		assert_eq!(standard_endpoint_region(&model.base_url), None);
		assert_eq!(
			endpoint_root(&model, "us-east-1"),
			"https://internal-proxy.local:8443/v1"
		);
		// Standard host re-roots at the resolved region.
		model.base_url = "https://bedrock-runtime.eu-west-1.amazonaws.com".to_string();
		assert_eq!(
			endpoint_root(&model, "us-east-1"),
			"https://bedrock-runtime.us-east-1.amazonaws.com"
		);
		// Empty base URL defaults to the regional endpoint.
		model.base_url = String::new();
		assert_eq!(
			endpoint_root(&model, "us-east-1"),
			"https://bedrock-runtime.us-east-1.amazonaws.com"
		);
	}

	// -- event-stream decoding ----------------------------------------------

	fn frame(message_type: &str, secondary: (&str, &str), payload: &str) -> Vec<u8> {
		let mut headers = Vec::new();
		let mut push_string = |name: &str, value: &str| {
			headers.push(name.len() as u8);
			headers.extend_from_slice(name.as_bytes());
			headers.push(7);
			headers.extend_from_slice(&(value.len() as u16).to_be_bytes());
			headers.extend_from_slice(value.as_bytes());
		};
		push_string(":message-type", message_type);
		push_string(secondary.0, secondary.1);
		push_string(":content-type", "application/json");

		let headers_len = headers.len();
		let total_len = 12 + headers_len + payload.len() + 4;
		let mut bytes = Vec::with_capacity(total_len);
		bytes.extend_from_slice(&(total_len as u32).to_be_bytes());
		bytes.extend_from_slice(&(headers_len as u32).to_be_bytes());
		let prelude_crc = crc32(&bytes);
		bytes.extend_from_slice(&prelude_crc.to_be_bytes());
		bytes.extend_from_slice(&headers);
		bytes.extend_from_slice(payload.as_bytes());
		let message_crc = crc32(&bytes);
		bytes.extend_from_slice(&message_crc.to_be_bytes());
		bytes
	}

	fn event_frame(event_type: &str, payload: &str) -> Vec<u8> {
		frame("event", (":event-type", event_type), payload)
	}

	fn exception_frame(exception_type: &str, payload: &str) -> Vec<u8> {
		frame("exception", (":exception-type", exception_type), payload)
	}

	#[test]
	fn decoder_reassembles_split_frames_and_validates_crc() {
		let mut decoder = EsDecoder::default();
		let mut frames = Vec::new();
		let bytes = event_frame("messageStart", r#"{"messageStart":{"role":"assistant"}}"#);
		// Feed one byte at a time to prove reassembly.
		for byte in &bytes {
			decoder
				.feed(std::slice::from_ref(byte), &mut frames)
				.unwrap();
		}
		assert_eq!(frames.len(), 1);
		assert_eq!(frames[0].message_type, "event");
		assert_eq!(frames[0].event_type.as_deref(), Some("messageStart"));

		// Corrupted CRC is rejected.
		let mut corrupted = bytes.clone();
		let last = corrupted.len() - 1;
		corrupted[last] ^= 0xff;
		let mut decoder = EsDecoder::default();
		let mut frames = Vec::new();
		assert!(decoder.feed(&corrupted, &mut frames).is_err());
	}

	#[test]
	fn session_emits_text_toolcall_and_usage_events() {
		let model = plain_model();
		let mut session = StreamSession::new(&model);
		let events = session
			.process_frame(
				&decode_frame(&event_frame(
					"messageStart",
					r#"{"messageStart":{"role":"assistant"}}"#,
				))
				.unwrap(),
			)
			.unwrap();
		assert!(matches!(events.first(), Some(Event::Start { .. })));

		let events = session
			.process_frame(
				&decode_frame(&event_frame(
					"contentBlockDelta",
					r#"{"contentBlockDelta":{"contentBlockIndex":0,"delta":{"text":"Hel"}}}"#,
				))
				.unwrap(),
			)
			.unwrap();
		// The first text delta opens the block before delivering the payload.
		assert!(matches!(
			events.first(),
			Some(Event::TextStart {
				content_index: 0,
				..
			})
		));
		assert!(
			events
				.iter()
				.any(|event| matches!(event, Event::TextDelta { delta, .. } if delta == "Hel"))
		);

		let events = session
			.process_frame(&decode_frame(&event_frame(
				"contentBlockStart",
				r#"{"contentBlockStart":{"contentBlockIndex":1,"start":{"toolUse":{"toolUseId":"t1","name":"ls"}}}}"#,
			)).unwrap())
			.unwrap();
		assert!(matches!(
			events.first(),
			Some(Event::ToolCallStart {
				content_index: 1,
				..
			})
		));

		let first_input = json!({"contentBlockDelta": {"contentBlockIndex": 1, "delta": {"toolUse": {"input": "{\"pa"}}}}).to_string();
		let events = session
			.process_frame(&decode_frame(&event_frame("contentBlockDelta", &first_input)).unwrap())
			.unwrap();
		assert!(
			matches!(events.first(), Some(Event::ToolCallDelta { delta, .. }) if delta == "{\"pa")
		);
		let second_input = json!({"contentBlockDelta": {"contentBlockIndex": 1, "delta": {"toolUse": {"input": "th\":\".\"}"}}}}).to_string();
		let events = session
			.process_frame(&decode_frame(&event_frame("contentBlockDelta", &second_input)).unwrap())
			.unwrap();
		assert!(
			matches!(events.first(), Some(Event::ToolCallDelta { delta, .. }) if delta == "th\":\".\"}")
		);

		let events = session
			.process_frame(
				&decode_frame(&event_frame(
					"contentBlockStop",
					r#"{"contentBlockStop":{"contentBlockIndex":1}}"#,
				))
				.unwrap(),
			)
			.unwrap();
		let Event::ToolCallEnd { tool_call, .. } = events.first().unwrap() else {
			panic!("expected toolcall_end");
		};
		assert_eq!(tool_call.arguments, json!({"path": "."}));

		let events = session
			.process_frame(
				&decode_frame(&event_frame(
					"contentBlockStop",
					r#"{"contentBlockStop":{"contentBlockIndex":0}}"#,
				))
				.unwrap(),
			)
			.unwrap();
		assert!(matches!(events.first(), Some(Event::TextEnd { content, .. }) if content == "Hel"));

		session
			.process_frame(&decode_frame(&event_frame(
				"metadata",
				r#"{"metadata":{"usage":{"inputTokens":100,"outputTokens":20,"totalTokens":120,"cacheReadInputTokens":5,"cacheDetails":[{"ttl":"1h","inputTokens":3}]}}}"#,
			)).unwrap())
			.unwrap();
		assert_eq!(session.message.usage.input, 100);
		assert_eq!(session.message.usage.output, 20);
		assert_eq!(session.message.usage.cache_read, 5);
		assert_eq!(session.message.usage.cache_write_1h, Some(3));
		assert_eq!(session.message.usage.total_tokens, 120);

		session
			.process_frame(
				&decode_frame(&event_frame(
					"messageStop",
					r#"{"messageStop":{"stopReason":"end_turn"}}"#,
				))
				.unwrap(),
			)
			.unwrap();
		assert_eq!(session.message.stop_reason, StopReason::Stop);
		assert_eq!(session.message.raw_stop_reason.as_deref(), Some("end_turn"));
	}

	#[test]
	fn session_redacted_reasoning_accumulates_base64() {
		let model = plain_model();
		let mut session = StreamSession::new(&model);
		session
			.process_frame(
				&decode_frame(&event_frame(
					"messageStart",
					r#"{"messageStart":{"role":"assistant"}}"#,
				))
				.unwrap(),
			)
			.unwrap();
		let first = session
			.process_frame(&decode_frame(&event_frame(
				"contentBlockDelta",
				r#"{"contentBlockDelta":{"contentBlockIndex":0,"delta":{"reasoningContent":{"redactedContent":"aGVs"}}}}"#,
			)).unwrap())
			.unwrap();
		assert!(matches!(
			first.first(),
			Some(Event::ThinkingStart {
				content_index: 0,
				..
			})
		));
		assert!(first.iter().any(|event| matches!(event, Event::ThinkingDelta { delta, .. } if delta == REDACTED_THINKING_PLACEHOLDER)));
		// The second chunk only accumulates bytes; the placeholder is emitted once.
		let second = session
			.process_frame(&decode_frame(&event_frame(
				"contentBlockDelta",
				r#"{"contentBlockDelta":{"contentBlockIndex":0,"delta":{"reasoningContent":{"redactedContent":"bG8="}}}}"#,
			)).unwrap())
			.unwrap();
		assert!(second.is_empty());
		session
			.process_frame(
				&decode_frame(&event_frame(
					"contentBlockStop",
					r#"{"contentBlockStop":{"contentBlockIndex":0}}"#,
				))
				.unwrap(),
			)
			.unwrap();
		let AssistantContent::Thinking(block) = &session.message.content[0] else {
			panic!("expected thinking block");
		};
		assert_eq!(block.redacted, Some(true));
		assert_eq!(block.thinking_signature.as_deref(), Some("aGVsbG8="));
		assert_eq!(block.thinking, REDACTED_THINKING_PLACEHOLDER);
	}

	#[test]
	fn exception_frames_format_with_prefixes_and_hint() {
		let model = plain_model();
		let mut session = StreamSession::new(&model);
		let error = session
			.process_frame(
				&decode_frame(&exception_frame(
					"ThrottlingException",
					r#"{"message":"Rate exceeded"}"#,
				))
				.unwrap(),
			)
			.unwrap_err();
		assert_eq!(error, "Throttling error: Rate exceeded");

		let error = format_bedrock_error(
			"ModelStreamErrorException",
			br#"{"message":"data retention mode 'default' is not available for this model"}"#,
		);
		assert!(error.starts_with("Model stream error: "));
		assert!(error.contains(BEDROCK_DATA_RETENTION_DOCS_URL));
	}

	#[test]
	fn truncated_payload_json_is_a_stream_error() {
		let model = plain_model();
		let mut session = StreamSession::new(&model);
		assert!(
			session
				.process_frame(&decode_frame(&event_frame("messageStart", "{oops")).unwrap())
				.is_err()
		);
	}
	// -- mock HTTP round trip -----------------------------------------------

	#[tokio::test]
	async fn mock_http_round_trip_streams_events() {
		// One full turn: text, tool call, usage, tool_use stop — all in the
		// binary event-stream framing, served by a local scripted server.
		let payloads: Vec<Vec<u8>> = vec![
			event_frame("messageStart", r#"{"messageStart":{"role":"assistant"}}"#),
			event_frame(
				"contentBlockDelta",
				r#"{"contentBlockDelta":{"contentBlockIndex":0,"delta":{"text":"Hel"}}}"#,
			),
			event_frame(
				"contentBlockDelta",
				r#"{"contentBlockDelta":{"contentBlockIndex":0,"delta":{"text":"lo"}}}"#,
			),
			event_frame(
				"contentBlockStart",
				r#"{"contentBlockStart":{"contentBlockIndex":1,"start":{"toolUse":{"toolUseId":"t1","name":"ls"}}}}"#,
			),
			{
				// Tool arguments chunk one: `{"pa` (JSON-encoded quotes).
				let payload = r#"{"contentBlockDelta":{"contentBlockIndex":1,"delta":{"toolUse":{"input":"{\"pa"}}}}"#;
				event_frame("contentBlockDelta", payload)
			},
			{
				// Chunk two closes the arguments object: `th":"."}`.
				let payload = r#"{"contentBlockDelta":{"contentBlockIndex":1,"delta":{"toolUse":{"input":"th\":\".\"}"}}}}"#;
				event_frame("contentBlockDelta", payload)
			},
			event_frame(
				"contentBlockStop",
				r#"{"contentBlockStop":{"contentBlockIndex":1}}"#,
			),
			event_frame(
				"contentBlockStop",
				r#"{"contentBlockStop":{"contentBlockIndex":0}}"#,
			),
			event_frame(
				"metadata",
				r#"{"metadata":{"usage":{"inputTokens":100,"outputTokens":7,"totalTokens":107}}}"#,
			),
			event_frame(
				"messageStop",
				r#"{"messageStop":{"stopReason":"tool_use"}}"#,
			),
		];
		let body: Vec<u8> = payloads.concat();
		let mut response = format!(
			"HTTP/1.1 200 OK\r\ncontent-type: application/vnd.amazon.eventstream\r\ncontent-length: {}\r\n\r\n",
			body.len()
		)
		.into_bytes();
		response.extend_from_slice(&body);

		let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
		let base = format!("http://{}", listener.local_addr().unwrap());
		tokio::spawn(async move {
			let Ok((mut socket, _)) = listener.accept().await else {
				return;
			};
			let mut buffer = Vec::new();
			let mut chunk = [0u8; 4096];
			loop {
				if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
					break;
				}
				if tokio::io::AsyncReadExt::read(&mut socket, &mut chunk)
					.await
					.unwrap_or(0) == 0
				{
					break;
				}
				buffer.extend_from_slice(&chunk);
			}
			let request = String::from_utf8_lossy(&buffer).to_string();
			assert!(
				request.starts_with("POST /model/mock-model%3A0/converse-stream HTTP/1.1"),
				"request line: {request}"
			);
			assert!(
				request.contains("authorization: Bearer test-bearer"),
				"auth header: {request}"
			);
			assert!(
				request.contains("accept: application/vnd.amazon.eventstream"),
				"accept header: {request}"
			);
			assert!(
				request.contains("x-amz-date:"),
				"amz date header: {request}"
			);
			let _ = tokio::io::AsyncWriteExt::write_all(&mut socket, &response).await;
			let _ = tokio::io::AsyncWriteExt::shutdown(&mut socket).await;
		});

		let model = Model::new(
			"mock-model:0",
			"Mock",
			Api::BEDROCK_CONVERSE_STREAM,
			"amazon-bedrock",
			&base,
		);
		let ctx = TranscriptContext {
			messages: vec![Message::System(system_with_tools()), Message::user("hi")],
		};
		let options = StreamOptions {
			api_key: Some("test-bearer".into()),
			..Default::default()
		};
		let mut stream = (stream_fn())(&model, &ctx, options);
		use futures::StreamExt;
		let mut events = Vec::new();
		while let Some(event) =
			tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
				.await
				.expect("in time")
		{
			let terminal = matches!(event, Event::Done { .. } | Event::Error { .. });
			events.push(event);
			if terminal {
				break;
			}
		}

		let Some(Event::Done { message, .. }) = events.last() else {
			panic!("done event, got {events:?}");
		};
		assert_eq!(message.stop_reason, StopReason::ToolUse);
		assert_eq!(message.raw_stop_reason.as_deref(), Some("tool_use"));
		let AssistantContent::Text(text) = &message.content[0] else {
			panic!("text at slot 0");
		};
		assert_eq!(text.text, "Hello");
		let AssistantContent::ToolCall(call) = &message.content[1] else {
			panic!("tool call at slot 1");
		};
		assert_eq!(call.id, "t1");
		assert_eq!(call.name, "ls");
		assert_eq!(call.arguments, json!({"path": "."}));
		assert_eq!(message.usage.input, 100);
		assert_eq!(message.usage.output, 7);
		assert_eq!(message.usage.total_tokens, 107);
		// Protocol event sequence: start/text/toolcall envelope in order.
		let kinds: Vec<&str> = events
			.iter()
			.map(|event| match event {
				Event::Start { .. } => "start",
				Event::TextStart { .. } => "text_start",
				Event::TextDelta { .. } => "text_delta",
				Event::TextEnd { .. } => "text_end",
				Event::ToolCallStart { .. } => "toolcall_start",
				Event::ToolCallDelta { .. } => "toolcall_delta",
				Event::ToolCallEnd { .. } => "toolcall_end",
				Event::Done { .. } => "done",
				_ => "other",
			})
			.collect();
		assert_eq!(
			kinds,
			vec![
				"start",
				"text_start",
				"text_delta",
				"text_delta",
				"toolcall_start",
				"toolcall_delta",
				"toolcall_delta",
				"toolcall_end",
				"text_end",
				"done",
			]
		);
	}
}
