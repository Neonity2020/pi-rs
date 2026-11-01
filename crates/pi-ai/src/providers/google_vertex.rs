//! Google Vertex AI adapter — port of `pi/packages/ai/src/api/
//! google-vertex.ts` (first slice). The wire protocol is identical to
//! `google-generative-ai` (same `GoogleSession`/`request_body`); the
//! differences are the endpoint and the credential:
//!
//! - **Express API key** (a real key in `options.api_key`): POST
//!   `https://aiplatform.googleapis.com/v1/publishers/google/models/{id}:
//!   streamGenerateContent?alt=sse` with `x-goog-api-key`.
//! - **ADC** (Application Default Credentials): POST
//!   `https://{location}-aiplatform.googleapis.com/v1/projects/{project}/
//!   locations/{location}/publishers/google/models/{id}:streamGenerateContent
//!   ?alt=sse` with `Authorization: Bearer <token>`. `GOOGLE_CLOUD_PROJECT`
//!   (or `GCLOUD_PROJECT`) and `GOOGLE_CLOUD_LOCATION` are required;
//!   `GOOGLE_APPLICATION_CREDENTIALS` selects the credential file —
//!   `service_account` JSON signs a JWT (RS256) and exchanges it at the
//!   JSON's `token_uri`; `authorized_user` JSON runs the refresh-token
//!   grant. The token is cached in memory until 60s before expiry.
//!
//! Deferred: metadata-server credentials (GCE/GKE), `toolChoice`,
//! constrained sampling, request retry, and the model-catalog
//! personalization the upstream provider shell performs.

use std::sync::Arc;

use serde_json::{Value, json};
use tokio::sync::mpsc::Sender;

use super::google_generative_ai::{GoogleSession, request_body};
use super::openai_completions::SseDecoder;
use crate::model::Model;
use crate::stream::{
	AssistantMessageEvent as Event, AssistantMessageEventStream, StreamFn, StreamOptions,
	TranscriptContext,
};
use crate::types::{AssistantMessage, StopReason};

/// Events buffered for the consumer before the producer task backpressures.
const EVENT_CHANNEL_CAPACITY: usize = 64;

const GCP_VERTEX_CREDENTIALS_MARKER: &str = "gcp-vertex-credentials";

/// Cached ADC token shared across streams in this process.
static ADC_TOKEN: std::sync::OnceLock<tokio::sync::Mutex<Option<AdcToken>>> =
	std::sync::OnceLock::new();

pub(crate) async fn forward(tx: &Sender<Event>, events: Vec<Event>) -> bool {
	for event in events {
		if tx.send(event).await.is_err() {
			return false;
		}
	}
	true
}

pub fn stream_fn() -> StreamFn {
	Arc::new(
		|model: &Model,
		 context: &TranscriptContext,
		 options: StreamOptions|
		 -> AssistantMessageEventStream { spawn_stream(model.clone(), context.clone(), options) },
	)
}

fn spawn_stream(
	model: Model,
	context: TranscriptContext,
	options: StreamOptions,
) -> AssistantMessageEventStream {
	let (tx, rx) = tokio::sync::mpsc::channel::<Event>(EVENT_CHANNEL_CAPACITY);
	tokio::spawn(async move {
		let signal = options.signal.clone();
		let result = tokio::select! {
			_ = signal.cancelled() => Err(Box::new(StreamFailure {
				reason: StopReason::Aborted,
				message: "Request was aborted".to_string(),
				partial: None,
			})),
			result = run_stream(&model, &context, &options, &tx) => result,
		};
		if let Err(failure) = result {
			let mut error = match failure.partial {
				Some(message) => message,
				None => AssistantMessage::error(&model, failure.reason, failure.message.clone()),
			};
			error.stop_reason = failure.reason;
			error.error_message = Some(failure.message);
			let _ = tx
				.send(Event::Error {
					reason: failure.reason,
					error,
				})
				.await;
		}
	});
	Box::pin(futures::stream::unfold(rx, |mut rx| async move {
		rx.recv().await.map(|event| (event, rx))
	}))
}

struct StreamFailure {
	reason: StopReason,
	message: String,
	partial: Option<AssistantMessage>,
}

async fn run_stream(
	model: &Model,
	context: &TranscriptContext,
	options: &StreamOptions,
	tx: &Sender<Event>,
) -> Result<(), Box<StreamFailure>> {
	let (url, auth) = resolve_endpoint(model, options).await.map_err(|message| {
		Box::new(StreamFailure {
			reason: StopReason::Error,
			message,
			partial: None,
		})
	})?;
	run_stream_inner(model, context, options, tx, url, auth).await
}

/// Resolved endpoint: the stream URL plus the credential to present.
enum Auth {
	ApiKey(String),
	Bearer(String),
}

impl Auth {
	fn apply(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
		match self {
			Auth::ApiKey(key) => request.header("x-goog-api-key", key),
			Auth::Bearer(token) => request.bearer_auth(token),
		}
	}
}

/// Upstream `resolveApiKey`: the key must be a real express key — the
/// credential-file marker and `<placeholder>` shapes fall through to ADC.
fn is_express_api_key(key: &str) -> bool {
	let key = key.trim();
	!key.is_empty() && key != GCP_VERTEX_CREDENTIALS_MARKER && !key.starts_with('<')
}

fn env_value(name: &str) -> Option<String> {
	std::env::var(name)
		.ok()
		.map(|v| v.trim().to_string())
		.filter(|v| !v.is_empty())
}

async fn resolve_endpoint(
	model: &Model,
	options: &StreamOptions,
) -> Result<(String, Auth), String> {
	let version = "v1";
	if let Some(key) = options
		.api_key
		.as_deref()
		.filter(|key| is_express_api_key(key))
	{
		return Ok((
			format!(
				"https://aiplatform.googleapis.com/{version}/publishers/google/models/{}:streamGenerateContent?alt=sse",
				model.id
			),
			Auth::ApiKey(key.to_string()),
		));
	}

	let project = env_value("GOOGLE_CLOUD_PROJECT")
		.or_else(|| env_value("GCLOUD_PROJECT"))
		.ok_or_else(|| {
			"Vertex AI requires a project ID. Set GOOGLE_CLOUD_PROJECT/GCLOUD_PROJECT.".to_string()
		})?;
	let location = env_value("GOOGLE_CLOUD_LOCATION")
		.ok_or_else(|| "Vertex AI requires a location. Set GOOGLE_CLOUD_LOCATION.".to_string())?;
	let token = adc_token().await?;
	// Custom base URL replaces the regional host prefix ({location} substituted).
	let prefix = match model.base_url.trim() {
		"" => format!("https://{location}-aiplatform.googleapis.com"),
		custom => custom.replace("{location}", &location),
	};
	Ok((
		format!(
			"{prefix}/{version}/projects/{project}/locations/{location}/publishers/google/models/{}:streamGenerateContent?alt=sse",
			model.id
		),
		Auth::Bearer(token),
	))
}

#[derive(Clone)]
struct AdcToken {
	value: String,
	/// Epoch ms after which the token must be refreshed.
	expires_at_ms: u64,
}

async fn adc_token() -> Result<String, String> {
	let cache = ADC_TOKEN.get_or_init(|| tokio::sync::Mutex::new(None));
	let mut cached = cache.lock().await;
	if let Some(token) = cached.as_ref()
		&& pi_ai_now_ms() + 60_000 < token.expires_at_ms
	{
		return Ok(token.value.clone());
	}
	let path = env_value("GOOGLE_APPLICATION_CREDENTIALS")
		.ok_or_else(|| {
			"Vertex AI ADC: set GOOGLE_APPLICATION_CREDENTIALS to a service-account or authorized-user JSON file."
				.to_string()
		})?;
	let file: Value = serde_json::from_str(
		&std::fs::read_to_string(&path).map_err(|error| format!("ADC file {path}: {error}"))?,
	)
	.map_err(|error| format!("ADC file {path} is not valid JSON: {error}"))?;
	let credential = match file.get("type").and_then(Value::as_str) {
		Some("service_account") => fetch_service_account_token(&file).await?,
		Some("authorized_user") => fetch_authorized_user_token(&file).await?,
		other => {
			return Err(format!(
				"ADC file {path}: unsupported credential type {:?} (service_account and authorized_user are supported)",
				other
			));
		}
	};
	*cached = Some(credential.clone());
	Ok(credential.value)
}

fn pi_ai_now_ms() -> u64 {
	std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.unwrap_or_default()
		.as_millis() as u64
}

/// OAuth token endpoint POST with urlencoded form body → `{access_token,
/// expires_in}`.
async fn post_token(token_uri: &str, body: String) -> Result<AdcToken, String> {
	let response = reqwest::Client::new()
		.post(token_uri)
		.header("content-type", "application/x-www-form-urlencoded")
		.body(body)
		.send()
		.await
		.map_err(|error| format!("token request failed: {error}"))?;
	let status = response.status();
	let text = response.text().await.unwrap_or_default();
	let value: Value = serde_json::from_str(&text).map_err(|error| {
		format!(
			"token response is not JSON ({error}): {}",
			&text[..text.len().min(200)]
		)
	})?;
	if !status.is_success() {
		return Err(format!(
			"token request returned {status}: {}",
			value["error_description"].as_str().unwrap_or("?")
		));
	}
	let access = value["access_token"]
		.as_str()
		.ok_or_else(|| "token response missing access_token".to_string())?;
	let expires_in = value["expires_in"].as_u64().unwrap_or(3600);
	Ok(AdcToken {
		value: access.to_string(),
		expires_at_ms: pi_ai_now_ms() + expires_in.saturating_mul(1000),
	})
}

/// `service_account` JSON: sign a RS256 JWT and exchange it
/// (`grant_type=urn:ietf:params:oauth:grant-type:jwt-bearer`).
async fn fetch_service_account_token(file: &Value) -> Result<AdcToken, String> {
	let client_email = file["client_email"]
		.as_str()
		.ok_or("ADC service_account missing client_email")?;
	let private_key = file["private_key"]
		.as_str()
		.ok_or("ADC service_account missing private_key")?;
	let token_uri = file["token_uri"]
		.as_str()
		.unwrap_or("https://oauth2.googleapis.com/token");
	let now = pi_ai_now_ms() / 1000;
	let b64 = |part: &[u8]| use_base64_url(part);
	let header = b64(br#"{"alg":"RS256","typ":"JWT"}"#.as_slice());
	let claims = b64(json!({
		"iss": client_email,
		"scope": "https://www.googleapis.com/auth/cloud-platform",
		"aud": token_uri,
		"iat": now,
		"exp": now + 3600,
	})
	.to_string()
	.as_bytes());
	let signing_input = format!("{header}.{claims}");
	let signature = sign_rs256(private_key, signing_input.as_bytes())?;
	let body = format!(
		"grant_type={}%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer&assertion={}.{}",
		"urn",
		signing_input,
		b64(&signature)
	);
	post_token(token_uri, body).await
}

/// `authorized_user` JSON (gcloud auth application-default login): the
/// refresh-token grant.
async fn fetch_authorized_user_token(file: &Value) -> Result<AdcToken, String> {
	let refresh_token = file["refresh_token"]
		.as_str()
		.ok_or("ADC authorized_user missing refresh_token")?;
	let client_id = file["client_id"]
		.as_str()
		.ok_or("ADC authorized_user missing client_id")?;
	let client_secret = file["client_secret"]
		.as_str()
		.ok_or("ADC authorized_user missing client_secret")?;
	let token_uri = file["token_uri"]
		.as_str()
		.unwrap_or("https://oauth2.googleapis.com/token");
	let body = format!(
		"grant_type=refresh_token&refresh_token={refresh_token}&client_id={client_id}&client_secret={client_secret}"
	);
	post_token(token_uri, body).await
}

fn use_base64_url(part: &[u8]) -> String {
	use base64::Engine as _;
	base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(part)
}

fn sign_rs256(private_key_pem: &str, input: &[u8]) -> Result<Vec<u8>, String> {
	use rsa::Pkcs1v15Sign;
	use rsa::pkcs1v15::{Signature, SigningKey};
	use rsa::signature::{SignatureEncoding, Signer};
	let _ = Pkcs1v15Sign::new_unprefixed;
	use rsa::pkcs1::DecodeRsaPrivateKey;
	use rsa::pkcs8::DecodePrivateKey;
	let key = rsa::RsaPrivateKey::from_pkcs1_pem(private_key_pem)
		.or_else(|_| rsa::RsaPrivateKey::from_pkcs8_pem(private_key_pem))
		.map_err(|error| format!("ADC private_key could not be parsed: {error}"))?;
	let signing_key = SigningKey::<sha2::Sha256>::new(key);
	let signature: Signature = signing_key.sign(input);
	Ok(signature.to_vec())
}

async fn run_stream_inner(
	model: &Model,
	context: &TranscriptContext,
	options: &StreamOptions,
	tx: &Sender<Event>,
	url: String,
	auth: Auth,
) -> Result<(), Box<StreamFailure>> {
	let mut response = auth
		.apply(reqwest::Client::new().post(url))
		.json(&request_body(model, context, options))
		.send()
		.await
		.map_err(|error| {
			Box::new(StreamFailure {
				reason: StopReason::Error,
				message: format!("request failed: {error}"),
				partial: None,
			})
		})?;
	let status = response.status();
	if !status.is_success() {
		let body = response.text().await.unwrap_or_default();
		let message = serde_json::from_str::<Value>(&body)
			.ok()
			.and_then(|value| value["error"]["message"].as_str().map(str::to_string))
			.unwrap_or_else(|| body.trim().to_string());
		return Err(Box::new(StreamFailure {
			reason: StopReason::Error,
			message: format!("provider returned {status}: {message}"),
			partial: None,
		}));
	}

	let mut session = GoogleSession::new(model);
	if !forward(
		tx,
		vec![Event::Start {
			partial: session.partial(),
		}],
	)
	.await
	{
		return Ok(());
	}
	let mut decoder = SseDecoder::default();
	loop {
		let chunk = match response.chunk().await {
			Ok(Some(chunk)) => chunk,
			Ok(None) => {
				let mut payloads = Vec::new();
				decoder.finish(&mut payloads);
				for payload in payloads {
					if session
						.process_payload_streamed(&payload, tx)
						.await
						.map_err(|message| {
							Box::new(StreamFailure {
								reason: StopReason::Error,
								message,
								partial: Some(session.partial()),
							})
						})? {
						return Ok(());
					}
				}
				break;
			}
			Err(error) => {
				return Err(Box::new(StreamFailure {
					reason: StopReason::Error,
					message: format!("stream read failed: {error}"),
					partial: Some(session.into_message()),
				}));
			}
		};
		let mut payloads = Vec::new();
		decoder.feed(&chunk, &mut payloads);
		for payload in payloads {
			if session
				.process_payload_streamed(&payload, tx)
				.await
				.map_err(|message| {
					Box::new(StreamFailure {
						reason: StopReason::Error,
						message,
						partial: Some(session.partial()),
					})
				})? {
				return Ok(());
			}
		}
		if decoder.is_done() {
			break;
		}
	}
	Err(Box::new(StreamFailure {
		reason: StopReason::Error,
		message: "Google stream ended without a finish reason".to_string(),
		partial: Some(session.into_message()),
	}))
}

#[cfg(test)]
mod tests {
	use super::*;

	fn test_model() -> Model {
		Model::new(
			"gemini-vertex",
			"gemini-vertex",
			"google-vertex",
			"google-vertex",
			"",
		)
	}

	#[test]
	fn express_api_key_selects_the_global_publisher_url() {
		let model = test_model();
		let options = StreamOptions {
			api_key: Some("AQ.ab8Rkey".into()),
			..Default::default()
		};
		// resolve_endpoint is async; block briefly.
		let (url, auth) = tokio::runtime::Runtime::new()
			.unwrap()
			.block_on(resolve_endpoint(&model, &options))
			.unwrap();
		assert_eq!(
			url,
			"https://aiplatform.googleapis.com/v1/publishers/google/models/gemini-vertex:streamGenerateContent?alt=sse"
		);
		assert!(matches!(auth, Auth::ApiKey(_)));
	}

	#[test]
	fn placeholder_keys_fall_through_to_adc() {
		let model = test_model();
		// The credential-file marker and <placeholder> shapes are not express keys.
		assert!(!is_express_api_key(GCP_VERTEX_CREDENTIALS_MARKER));
		assert!(!is_express_api_key("<to-be-filled>"));
		assert!(is_express_api_key("AQ.ab8Rkey"));
		let _ = model;
	}

	#[test]
	fn jwt_signing_produces_a_verifyable_rs256_signature() {
		// Round-trip through the matching public key.
		use rsa::pkcs1::EncodeRsaPrivateKey;
		use rsa::pkcs8::LineEnding;
		let mut rng = rsa::rand_core::OsRng;
		let private_key = rsa::RsaPrivateKey::new(&mut rng, 2048).unwrap();
		let pem = private_key
			.to_pkcs1_pem(LineEnding::LF)
			.unwrap()
			.to_string();
		let signature = sign_rs256(&pem, b"signing-input").unwrap();
		let public_key = rsa::RsaPublicKey::from(&private_key);
		let verifying_key = rsa::pkcs1v15::VerifyingKey::<sha2::Sha256>::new(public_key);
		use rsa::signature::Verifier;
		let signature = rsa::pkcs1v15::Signature::try_from(signature.as_slice()).unwrap();
		assert!(verifying_key.verify(b"signing-input", &signature).is_ok());
		assert!(verifying_key.verify(b"other-input", &signature).is_err());
	}
}
