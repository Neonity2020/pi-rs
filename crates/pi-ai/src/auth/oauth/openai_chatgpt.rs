//! OpenAI Responses API token sharing through Sign in with ChatGPT — port of
//! `pi/packages/ai/src/auth/oauth/openai-chatgpt.ts`.
//!
//! This public-client flow uses no client secret and sends the resulting user
//! access token directly to api.openai.com.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;

use super::{now_ms, send_request};
use crate::auth::callback::{oauth_error_html, oauth_success_html, parse_query, write_page};
use crate::auth::pkce::{generate_pkce, getrandom_fill};
use crate::auth::types::{
	AuthError, AuthEvent, AuthInteraction, AuthPrompt, AuthPromptKind, LoginOptions, ModelAuth,
	OAuthCredential, OAuthFlow,
};

// every login registers a new client with this ID; OpenAI returns the issued client ID in the callback
const DYNAMIC_CLIENT_ID: &str = "dynamic_agent_client";
const AGENT_NAME_HINT: &str = "Pi";
const AUTHORIZE_URL: &str = "https://auth.openai.com/api/accounts/authorize";
const TOKEN_URL: &str = "https://auth.openai.com/api/accounts/oauth/token";
const RESOURCE: &str = "https://api.openai.com/v1";
const CALLBACK_HOST_ENV: &str = "PI_OAUTH_CALLBACK_HOST";
const CALLBACK_PORT: u16 = 1455;
const CALLBACK_PATH: &str = "/auth/callback";
const REDIRECT_URI: &str = "http://127.0.0.1:1455/auth/callback";
const DIRECT_TOKEN_SCOPE: &str = "chatgpt.tokens.use.direct";
const SCOPE: &str = "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
// Refresh this long before the real expiry so a request never starts with a token about to expire.
const EXPIRY_MARGIN_MS: i64 = 3 * 60 * 1000;

fn callback_host() -> String {
	std::env::var(CALLBACK_HOST_ENV)
		.ok()
		.filter(|value| !value.is_empty())
		.unwrap_or_else(|| "127.0.0.1".to_string())
}

#[derive(Debug, Clone, PartialEq)]
struct AuthorizationResult {
	code: String,
	client_id: String,
}

fn is_uuid(value: &str) -> bool {
	let bytes = value.as_bytes();
	if bytes.len() != 36 {
		return false;
	}
	for (index, byte) in bytes.iter().enumerate() {
		match index {
			8 | 13 | 18 | 23 => {
				if *byte != b'-' {
					return false;
				}
			}
			_ => {
				if !byte.is_ascii_hexdigit() {
					return false;
				}
			}
		}
	}
	true
}

/// OpenAI identifies each installation ("agent host") by a stable URI such as `urn:uuid:<uuid>`.
fn agent_host_id(device_id: Option<&str>) -> Result<String, AuthError> {
	match device_id.filter(|device_id| is_uuid(device_id)) {
		Some(device_id) => Ok(format!("urn:uuid:{}", device_id.to_lowercase())),
		None => Err(AuthError::Login(
			"Sign in with ChatGPT requires a device ID (UUID) for this installation".to_string(),
		)),
	}
}

fn random_value() -> Result<String, AuthError> {
	let mut bytes = [0u8; 32];
	getrandom_fill(&mut bytes)?;
	Ok(base64url_encode(&bytes))
}

fn base64url_encode(bytes: &[u8]) -> String {
	use base64::Engine;
	base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Pure half of upstream `authorizationResultFromCallback`.
fn authorization_result_from_callback(
	params: &[(String, String)],
	expected_state: &str,
) -> Result<AuthorizationResult, AuthError> {
	let find = |key: &str| {
		params
			.iter()
			.find(|(name, _)| name == key)
			.map(|(_, value)| value.clone())
	};
	let Some(code) = find("code").filter(|code| !code.is_empty()) else {
		return Err(AuthError::Login("Missing authorization code".to_string()));
	};
	let Some(state) = find("state").filter(|state| !state.is_empty()) else {
		return Err(AuthError::Login("Missing OAuth state".to_string()));
	};
	if state != expected_state {
		return Err(AuthError::Login("OAuth state mismatch".to_string()));
	}
	let Some(client_id) = find("client_id")
		.map(|value| value.trim().to_string())
		.filter(|client_id| !client_id.is_empty())
	else {
		return Err(AuthError::Login(
			"OpenAI OAuth registration callback did not contain an issued client ID".to_string(),
		));
	};
	Ok(AuthorizationResult { code, client_id })
}

/// Pure half of upstream `authorizationResultFromManualInput`.
fn authorization_result_from_manual_input(
	input: &str,
	expected_state: &str,
) -> Result<AuthorizationResult, AuthError> {
	let url = reqwest::Url::parse(input.trim()).map_err(|_| {
		AuthError::Login("Paste the full callback URL from the browser".to_string())
	})?;
	let expected = reqwest::Url::parse(REDIRECT_URI).expect("REDIRECT_URI parses");
	if url.origin() != expected.origin() || url.path() != expected.path() {
		return Err(AuthError::Login(format!(
			"The pasted callback URL must start with {REDIRECT_URI}"
		)));
	}
	let params = parse_query(url.query().unwrap_or_default());
	if let Some((_, error)) = params.iter().find(|(name, _)| name == "error") {
		return Err(AuthError::Login(format!(
			"ChatGPT authorization failed: {error}"
		)));
	}
	authorization_result_from_callback(&params, expected_state)
}

/// One-shot loopback callback server mirroring upstream `startCallbackServer`.
/// The ChatGPT flow reads the issued `client_id` from the callback URL, which
/// the shared [`crate::auth::callback`] server does not deliver, so — like the
/// TS original — this flow owns its server. Every claimed callback resolves
/// the flow; invalid requests only get a 400 page and serving continues.
struct ChatGptCallbackServer {
	outcome: Mutex<Option<std::sync::mpsc::Receiver<Result<AuthorizationResult, String>>>>,
	shutdown: Arc<AtomicBool>,
}

impl ChatGptCallbackServer {
	fn start(
		expected_state: String,
		signal: tokio_util::sync::CancellationToken,
	) -> Result<Self, AuthError> {
		if signal.is_cancelled() {
			return Err(AuthError::Login("Login cancelled".to_string()));
		}
		let listener = TcpListener::bind((callback_host().as_str(), CALLBACK_PORT)).map_err(|error| {
			if error.kind() == std::io::ErrorKind::AddrInUse {
				AuthError::Login(format!(
					"Port {CALLBACK_PORT} is in use, probably by an unfinished login in another pi session or by the Codex CLI. Cancel that login and try again."
				))
			} else {
				AuthError::Login(format!("OAuth callback server failed to bind: {error}"))
			}
		})?;
		listener.set_nonblocking(true).ok();
		let (sender, receiver) = std::sync::mpsc::channel::<Result<AuthorizationResult, String>>();
		let shutdown = Arc::new(AtomicBool::new(false));
		let thread_shutdown = Arc::clone(&shutdown);
		std::thread::spawn(move || {
			loop {
				if thread_shutdown.load(Ordering::SeqCst) {
					return;
				}
				if let Ok((stream, _addr)) = listener.accept()
					&& handle_connection(stream, &expected_state, &sender)
				{
					return;
				}
				std::thread::sleep(std::time::Duration::from_millis(20));
			}
		});
		Ok(Self {
			outcome: Mutex::new(Some(receiver)),
			shutdown,
		})
	}

	async fn wait(&self) -> Result<AuthorizationResult, AuthError> {
		let receiver = self.outcome.lock().unwrap().take();
		let Some(receiver) = receiver else {
			return Err(AuthError::Login("Login cancelled".to_string()));
		};
		let outcome = tokio::task::spawn_blocking(move || {
			receiver
				.recv()
				.unwrap_or_else(|_| Err("Login cancelled".to_string()))
		})
		.await
		.map_err(|error| AuthError::Login(error.to_string()))?;
		outcome.map_err(AuthError::Login)
	}

	fn close(&self) {
		self.shutdown.store(true, Ordering::SeqCst);
	}
}

/// Handle one browser GET; `true` = the flow's outcome was resolved.
fn handle_connection(
	stream: std::net::TcpStream,
	expected_state: &str,
	sender: &std::sync::mpsc::Sender<Result<AuthorizationResult, String>>,
) -> bool {
	let Ok(stream_clone) = stream.try_clone() else {
		write_page(
			stream,
			500,
			&oauth_error_html("Internal error while processing the callback.", None),
		);
		return false;
	};
	let mut reader = BufReader::new(stream_clone);
	let mut request_line = String::new();
	if reader.read_line(&mut request_line).is_err() {
		write_page(
			stream,
			500,
			&oauth_error_html("Internal error while processing the callback.", None),
		);
		return false;
	}
	// Drain headers (bounded).
	let mut total = 0usize;
	loop {
		let mut line = String::new();
		match reader.read_line(&mut line) {
			Ok(0) | Err(_) => break,
			Ok(n) => {
				total += n;
				if total > 16 * 1024 || line == "\r\n" || line == "\n" {
					break;
				}
			}
		}
	}
	let target = request_line.split_whitespace().nth(1).unwrap_or("/");
	let (route, query) = match target.split_once('?') {
		Some((route, query)) => (route, query),
		None => (target, ""),
	};
	let params = parse_query(query);
	if route != CALLBACK_PATH {
		write_page(
			stream,
			404,
			&oauth_error_html("Callback route not found.", None),
		);
		return false;
	}
	if let Some((_, error)) = params.iter().find(|(name, _)| name == "error") {
		write_page(
			stream,
			400,
			&oauth_error_html(
				"ChatGPT was not connected.",
				Some(&format!("Error: {error}")),
			),
		);
		let _ = sender.send(Err(format!("ChatGPT authorization failed: {error}")));
		return true;
	}
	match authorization_result_from_callback(&params, expected_state) {
		Ok(result) => {
			write_page(
				stream,
				200,
				&oauth_success_html("ChatGPT authentication completed. You can close this window."),
			);
			let _ = sender.send(Ok(result));
			true
		}
		Err(error) => {
			write_page(stream, 400, &oauth_error_html(&error.to_string(), None));
			false
		}
	}
}

const TOKEN_HEADERS: &[(&str, &str)] = &[
	("accept", "application/json"),
	("content-type", "application/x-www-form-urlencoded"),
];

async fn request_token(
	form: Vec<(String, String)>,
	signal: tokio_util::sync::CancellationToken,
) -> Result<serde_json::Value, AuthError> {
	let response = send_request(
		reqwest::Method::POST,
		TOKEN_URL,
		TOKEN_HEADERS,
		Some(crate::auth::http::encode_form(&form)),
		signal,
	)
	.await?;
	let status = response.status();
	let text = response
		.text()
		.await
		.map_err(|error| AuthError::Login(error.to_string()))?;
	if !status.is_success() {
		let body_or_status = if text.is_empty() {
			status.canonical_reason().unwrap_or("").to_string()
		} else {
			text
		};
		return Err(AuthError::Login(format!(
			"OpenAI OAuth token request failed ({}): {body_or_status}",
			status.as_u16()
		)));
	}
	let data: serde_json::Value =
		serde_json::from_str(&text).map_err(|error| AuthError::Login(error.to_string()))?;
	if !data.is_object() {
		return Err(AuthError::Login(
			"OpenAI OAuth token response must be an object".to_string(),
		));
	}
	Ok(data)
}

fn require_token_string<'a>(
	token: &'a serde_json::Value,
	field: &str,
) -> Result<&'a str, AuthError> {
	match token.get(field).and_then(|value| value.as_str()) {
		Some(value) if !value.trim().is_empty() => Ok(value),
		_ => Err(AuthError::Login(format!(
			"OpenAI OAuth token response has invalid {field}"
		))),
	}
}

/// Pure half of upstream `credentialFromTokenResponse`.
fn credential_from_token_response(
	token: &serde_json::Value,
	client_id: &str,
) -> Result<OAuthCredential, AuthError> {
	let access = require_token_string(token, "access_token")?.to_string();
	let refresh = require_token_string(token, "refresh_token")?.to_string();
	let scope = require_token_string(token, "scope")?;
	if !token
		.get("expires_in")
		.and_then(|value| value.as_f64())
		.is_some_and(|value| value.is_finite() && value > 0.0)
	{
		return Err(AuthError::Login(
			"OpenAI OAuth token response has invalid expires_in".to_string(),
		));
	}
	let expires_in = token
		.get("expires_in")
		.and_then(|value| value.as_f64())
		.unwrap_or_default();
	let scopes = scope
		.split_whitespace()
		.filter(|part| !part.is_empty())
		.map(str::to_string)
		.collect::<Vec<_>>();
	if !scopes.iter().any(|scope| scope == DIRECT_TOKEN_SCOPE) {
		return Err(AuthError::Login(format!(
			"OpenAI OAuth grant did not include {DIRECT_TOKEN_SCOPE}"
		)));
	}
	let mut extra = BTreeMap::new();
	extra.insert(
		"clientId".to_string(),
		serde_json::Value::String(client_id.to_string()),
	);
	extra.insert(
		"scopes".to_string(),
		serde_json::Value::Array(scopes.into_iter().map(serde_json::Value::String).collect()),
	);
	Ok(OAuthCredential {
		access,
		refresh,
		expires: now_ms() + (expires_in * 1000.0) as i64 - EXPIRY_MARGIN_MS,
		extra,
	})
}

async fn exchange_authorization_code(
	code: &str,
	verifier: &str,
	client_id: &str,
	signal: tokio_util::sync::CancellationToken,
) -> Result<OAuthCredential, AuthError> {
	let token = request_token(
		vec![
			("grant_type".to_string(), "authorization_code".to_string()),
			("client_id".to_string(), client_id.to_string()),
			("code".to_string(), code.to_string()),
			("code_verifier".to_string(), verifier.to_string()),
			("redirect_uri".to_string(), REDIRECT_URI.to_string()),
			("resource".to_string(), RESOURCE.to_string()),
		],
		signal,
	)
	.await?;
	// Pi does not use the ID token to identify the user or read profile data.
	// Keep the presence check as part of the token-response contract.
	if !token
		.get("id_token")
		.and_then(|value| value.as_str())
		.is_some_and(|value| !value.trim().is_empty())
	{
		return Err(AuthError::Login(
			"OpenAI OAuth token response did not contain an ID token".to_string(),
		));
	}
	credential_from_token_response(&token, client_id)
}

async fn refresh_access_token(
	credential: &OAuthCredential,
	signal: tokio_util::sync::CancellationToken,
) -> Result<OAuthCredential, AuthError> {
	let client_id = credential
		.extra
		.get("clientId")
		.and_then(|value| value.as_str())
		.map(str::trim)
		.filter(|value| !value.is_empty());
	let Some(client_id) = client_id else {
		return Err(AuthError::Login(
			"Stored OpenAI OAuth credential does not contain an issued client ID; reconnect ChatGPT"
				.to_string(),
		));
	};
	let token = request_token(
		vec![
			("grant_type".to_string(), "refresh_token".to_string()),
			("client_id".to_string(), client_id.to_string()),
			("refresh_token".to_string(), credential.refresh.clone()),
			("resource".to_string(), RESOURCE.to_string()),
		],
		signal,
	)
	.await?;
	credential_from_token_response(&token, client_id)
}

async fn login_openai_chatgpt(
	interaction: &dyn AuthInteraction,
	options: &LoginOptions,
) -> Result<OAuthCredential, AuthError> {
	let host_id = agent_host_id(options.get_device_id.as_deref())?;
	let (verifier, challenge) = generate_pkce()?;
	let state = random_value()?;
	let nonce = random_value()?;
	// Without this server, the browser's callback would reach whatever else holds the port (another
	// pending login or the Codex CLI), which rejects it as a state mismatch. Fail with a clear error instead.
	let callback = ChatGptCallbackServer::start(state.clone(), interaction.signal())?;

	let inner: Result<OAuthCredential, AuthError> = async {
		let query = [
			("client_id", DYNAMIC_CLIENT_ID.to_string()),
			(
				"agent_name_hint",
				options.agent_name.clone().unwrap_or_else(|| AGENT_NAME_HINT.to_string()),
			),
			("ext_agent_host_id", host_id),
			("response_type", "code".to_string()),
			("redirect_uri", REDIRECT_URI.to_string()),
			("resource", RESOURCE.to_string()),
			("scope", SCOPE.to_string()),
			("state", state.clone()),
			("code_challenge", challenge),
			("code_challenge_method", "S256".to_string()),
			("nonce", nonce),
		];
		let authorization_url = format!(
			"{AUTHORIZE_URL}?{}",
			crate::auth::http::encode_form(&query.iter().map(|(k, v)| (k.to_string(), v.clone())).collect::<Vec<_>>())
		);
		interaction.notify(AuthEvent::AuthUrl {
			url: authorization_url,
			instructions: Some(
				"Complete sign-in in your browser. If the callback does not complete, paste the final redirect URL here."
					.to_string(),
			),
		});

		let wait = callback.wait();
		tokio::pin!(wait);
		let manual_prompt = async {
			let input = interaction
				.prompt(AuthPrompt {
					kind: AuthPromptKind::ManualCode {
						message: "Complete login in your browser, or paste the final redirect URL here:"
							.to_string(),
						placeholder: Some(REDIRECT_URI.to_string()),
					},
					signal: interaction.signal().child_token(),
				})
				.await?;
			authorization_result_from_manual_input(&input, &state)
		};
		tokio::pin!(manual_prompt);
		let result = tokio::select! {
			outcome = &mut wait => match outcome {
				Ok(result) => result,
				Err(message) => return Err(message),
			},
			manual = &mut manual_prompt => manual?,
		};
		interaction.notify(AuthEvent::Progress {
			message: "Exchanging authorization code for tokens...".to_string(),
		});
		exchange_authorization_code(&result.code, &verifier, &result.client_id, interaction.signal()).await
	}
	.await;
	let outcome = match inner {
		Ok(credential) => Ok(credential),
		Err(_) if interaction.signal().is_cancelled() => {
			Err(AuthError::Login("Login cancelled".to_string()))
		}
		Err(error) => Err(error),
	};
	callback.close();
	outcome
}

pub struct OpenAIChatGptOAuth;

impl OAuthFlow for OpenAIChatGptOAuth {
	fn name(&self) -> &str {
		"OpenAI (ChatGPT subscription)"
	}

	fn is_subscription(&self) -> bool {
		true
	}

	fn login<'a>(
		&'a self,
		interaction: &'a dyn AuthInteraction,
		options: &'a LoginOptions,
	) -> BoxFuture<'a, Result<OAuthCredential, AuthError>> {
		Box::pin(async move { login_openai_chatgpt(interaction, options).await })
	}

	fn refresh(
		&self,
		credential: &OAuthCredential,
	) -> BoxFuture<'_, Result<OAuthCredential, AuthError>> {
		let credential = credential.clone();
		Box::pin(async move {
			refresh_access_token(&credential, tokio_util::sync::CancellationToken::new()).await
		})
	}

	fn to_auth(&self, credential: &OAuthCredential) -> Result<ModelAuth, AuthError> {
		Ok(ModelAuth {
			api_key: Some(credential.access.clone()),
			..Default::default()
		})
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use serde_json::json;

	#[test]
	fn uuid_check_matches_the_upstream_pattern() {
		assert!(is_uuid("6a35d5c4-9c60-4a4e-8b1f-2b6d0e5f1234"));
		assert!(is_uuid("6A35D5C4-9C60-4A4E-8B1F-2B6D0E5F1234"));
		assert!(!is_uuid("6a35d5c4-9c60-4a4e-8b1f-2b6d0e5f123"));
		assert!(!is_uuid("6a35d5c49c60-4a4e-8b1f-2b6d0e5f1234"));
		assert!(!is_uuid("6a35d5c4-9c60-4a4e-8b1f-2b6d0e5f123g"));
		assert!(!is_uuid(""));
	}

	#[test]
	fn agent_host_id_requires_a_uuid_device_id() {
		let error = agent_host_id(None).unwrap_err();
		assert_eq!(
			error.to_string(),
			"Sign in with ChatGPT requires a device ID (UUID) for this installation"
		);
		let error = agent_host_id(Some("not-a-uuid")).unwrap_err();
		assert_eq!(
			error.to_string(),
			"Sign in with ChatGPT requires a device ID (UUID) for this installation"
		);
		assert_eq!(
			agent_host_id(Some("6A35D5C4-9C60-4A4E-8B1F-2B6D0E5F1234")).unwrap(),
			"urn:uuid:6a35d5c4-9c60-4a4e-8b1f-2b6d0e5f1234"
		);
	}

	#[test]
	fn callback_result_requires_code_state_and_issued_client_id() {
		let params = |query: &str| parse_query(query);
		assert_eq!(
			authorization_result_from_callback(&params("code=c&state=s&client_id=issued"), "s")
				.unwrap(),
			AuthorizationResult {
				code: "c".to_string(),
				client_id: "issued".to_string()
			}
		);
		// client_id is trimmed but still required.
		assert_eq!(
			authorization_result_from_callback(
				&params("code=c&state=s&client_id=%20issued%20"),
				"s"
			)
			.unwrap()
			.client_id,
			"issued"
		);
		assert_eq!(
			authorization_result_from_callback(&params("state=s&client_id=issued"), "s")
				.unwrap_err()
				.to_string(),
			"Missing authorization code"
		);
		assert_eq!(
			authorization_result_from_callback(&params("code=&state=s&client_id=issued"), "s")
				.unwrap_err()
				.to_string(),
			"Missing authorization code"
		);
		assert_eq!(
			authorization_result_from_callback(&params("code=c&client_id=issued"), "s")
				.unwrap_err()
				.to_string(),
			"Missing OAuth state"
		);
		assert_eq!(
			authorization_result_from_callback(&params("code=c&state=other&client_id=issued"), "s")
				.unwrap_err()
				.to_string(),
			"OAuth state mismatch"
		);
		assert_eq!(
			authorization_result_from_callback(&params("code=c&state=s"), "s")
				.unwrap_err()
				.to_string(),
			"OpenAI OAuth registration callback did not contain an issued client ID"
		);
		assert_eq!(
			authorization_result_from_callback(&params("code=c&state=s&client_id=%20%20"), "s")
				.unwrap_err()
				.to_string(),
			"OpenAI OAuth registration callback did not contain an issued client ID"
		);
	}

	#[test]
	fn manual_input_validates_prefix_error_and_fields() {
		let state = "st";
		// Not a URL at all.
		assert_eq!(
			authorization_result_from_manual_input("not a url", state)
				.unwrap_err()
				.to_string(),
			"Paste the full callback URL from the browser"
		);
		// Wrong origin / path.
		assert_eq!(
			authorization_result_from_manual_input(
				"http://127.0.0.1:9999/auth/callback?code=c&state=st&client_id=i",
				state
			)
			.unwrap_err()
			.to_string(),
			"The pasted callback URL must start with http://127.0.0.1:1455/auth/callback"
		);
		assert_eq!(
			authorization_result_from_manual_input(
				"http://127.0.0.1:1455/other?code=c&state=st&client_id=i",
				state
			)
			.unwrap_err()
			.to_string(),
			"The pasted callback URL must start with http://127.0.0.1:1455/auth/callback"
		);
		// The error parameter wins over the rest.
		assert_eq!(
			authorization_result_from_manual_input(
				&format!("{REDIRECT_URI}?error=access_denied"),
				state
			)
			.unwrap_err()
			.to_string(),
			"ChatGPT authorization failed: access_denied"
		);
		// Happy path (whitespace around the paste is trimmed).
		let result = authorization_result_from_manual_input(
			&format!("  {REDIRECT_URI}?code=c&state=st&client_id=i  "),
			state,
		)
		.unwrap();
		assert_eq!(
			result,
			AuthorizationResult {
				code: "c".to_string(),
				client_id: "i".to_string()
			}
		);
	}

	#[test]
	fn token_response_validation_order_and_messages() {
		assert_eq!(
			credential_from_token_response(&json!({}), "issued")
				.unwrap_err()
				.to_string(),
			"OpenAI OAuth token response has invalid access_token"
		);
		assert_eq!(
			credential_from_token_response(&json!({"access_token": "a"}), "issued")
				.unwrap_err()
				.to_string(),
			"OpenAI OAuth token response has invalid refresh_token"
		);
		assert_eq!(
			credential_from_token_response(
				&json!({"access_token": "a", "refresh_token": "r"}),
				"issued"
			)
			.unwrap_err()
			.to_string(),
			"OpenAI OAuth token response has invalid scope"
		);
		assert_eq!(
			credential_from_token_response(
				&json!({"access_token": "a", "refresh_token": "r", "scope": "openid"}),
				"issued"
			)
			.unwrap_err()
			.to_string(),
			"OpenAI OAuth token response has invalid expires_in"
		);
		assert_eq!(
			credential_from_token_response(
				&json!({"access_token": "a", "refresh_token": "r", "scope": "openid", "expires_in": 0}),
				"issued"
			)
			.unwrap_err()
			.to_string(),
			"OpenAI OAuth token response has invalid expires_in"
		);
		assert_eq!(
			credential_from_token_response(
				&json!({"access_token": "a", "refresh_token": "r", "scope": "openid", "expires_in": 100}),
				"issued"
			)
			.unwrap_err()
			.to_string(),
			"OpenAI OAuth grant did not include chatgpt.tokens.use.direct"
		);

		let credential = credential_from_token_response(
			&json!({
				"access_token": "a",
				"refresh_token": "r",
				"scope": "openid  chatgpt.tokens.use.direct profile",
				"expires_in": 100
			}),
			"issued",
		)
		.unwrap();
		assert_eq!(credential.access, "a");
		assert_eq!(credential.refresh, "r");
		assert_eq!(
			credential.extra.get("clientId").and_then(|v| v.as_str()),
			Some("issued")
		);
		assert_eq!(
			credential.extra.get("scopes"),
			Some(&json!(["openid", "chatgpt.tokens.use.direct", "profile"]))
		);
		let now = now_ms();
		assert_eq!(credential.expires, now + 100 * 1000 - EXPIRY_MARGIN_MS);
	}

	#[test]
	fn id_token_presence_is_part_of_the_exchange_contract() {
		// Mirrors exchange_authorization_code's extra check.
		let token = json!({"access_token": "a", "id_token": ""});
		assert!(
			!token
				.get("id_token")
				.and_then(|value| value.as_str())
				.is_some_and(|value| !value.trim().is_empty())
		);
	}

	#[tokio::test]
	async fn refresh_requires_the_stored_client_id() {
		let credential = OAuthCredential {
			access: "a".to_string(),
			refresh: "r".to_string(),
			expires: 0,
			extra: Default::default(),
		};
		let error = refresh_access_token(&credential, tokio_util::sync::CancellationToken::new())
			.await
			.unwrap_err();
		assert_eq!(
			error.to_string(),
			"Stored OpenAI OAuth credential does not contain an issued client ID; reconnect ChatGPT"
		);
	}

	#[test]
	fn random_values_are_base64url_and_unique() {
		let value = random_value().unwrap();
		assert_eq!(value.len(), 43);
		assert!(!value.contains('+') && !value.contains('/') && !value.contains('='));
		assert_ne!(value, random_value().unwrap());
	}
}
