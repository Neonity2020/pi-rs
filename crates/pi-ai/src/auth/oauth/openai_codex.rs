//! OpenAI Codex (ChatGPT OAuth) flow — port of
//! `pi/packages/ai/src/auth/oauth/openai-codex.ts` (browser + device flow).

use futures::future::BoxFuture;

use super::wait_for_callback_or_manual_input;
use crate::auth::callback::{CallbackServerOptions, OAuthCallbackServer, parse_query};
use crate::auth::device::{DevicePollResult, poll_oauth_device_code_flow};
use crate::auth::pkce::{generate_pkce, getrandom_fill};
use crate::auth::types::{
	AuthError, AuthEvent, AuthInteraction, AuthPromptKind, LoginOptions, ModelAuth,
	OAuthCredential, OAuthFlow, SelectOption,
};

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const DEVICE_USER_CODE_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/usercode";
const DEVICE_TOKEN_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
const DEVICE_VERIFICATION_URI: &str = "https://auth.openai.com/codex/device";
const DEVICE_REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";
const DEVICE_CODE_TIMEOUT_SECONDS: u64 = 15 * 60;
const OPENAI_CODEX_BROWSER_LOGIN_METHOD: &str = "browser";
const OPENAI_CODEX_DEVICE_CODE_LOGIN_METHOD: &str = "device_code";
const SCOPE: &str = "openid profile email offline_access";
const JWT_CLAIM_PATH: &str = "https://api.openai.com/auth";

const CALLBACK_HOST_ENV: &str = "PI_OAUTH_CALLBACK_HOST";

#[derive(Debug)]
struct OAuthToken {
	access: String,
	refresh: String,
	expires: i64,
}

#[derive(Debug)]
struct DeviceAuthInfo {
	device_auth_id: String,
	user_code: String,
	interval_seconds: f64,
}

#[derive(Debug)]
struct DeviceTokenSuccess {
	authorization_code: String,
	code_verifier: String,
}

fn get_callback_host() -> String {
	std::env::var(CALLBACK_HOST_ENV)
		.ok()
		.filter(|value| !value.is_empty())
		.unwrap_or_else(|| "127.0.0.1".to_string())
}

fn create_state() -> Result<String, AuthError> {
	let mut bytes = [0u8; 16];
	getrandom_fill(&mut bytes)?;
	Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn parse_authorization_input(input: &str) -> (Option<String>, Option<String>) {
	let value = input.trim();
	if value.is_empty() {
		return (None, None);
	}

	if let Ok(url) = reqwest::Url::parse(value) {
		let find = |key: &str| {
			url.query_pairs()
				.find(|(name, _)| name == key)
				.map(|(_, value)| value.into_owned())
		};
		return (find("code"), find("state"));
	}

	if value.contains('#') {
		// Upstream `value.split("#", 2)`: only the first two parts survive.
		let mut parts = value.split('#');
		let code = parts.next().unwrap_or_default().to_string();
		let state = parts.next().map(str::to_string);
		return (Some(code), state);
	}

	if value.contains("code=") {
		let params = parse_query(value);
		let find = |key: &str| {
			params
				.iter()
				.find(|(name, _)| name == key)
				.map(|(_, value)| value.clone())
		};
		return (find("code"), find("state"));
	}

	(Some(value.to_string()), None)
}

/// Lenient base64 for JWT payload segments (Node's `atob` accepts both
/// alphabets and optional padding).
fn decode_jwt_payload(segment: &str) -> Option<serde_json::Value> {
	use base64::Engine;
	use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
	static ENGINE: std::sync::OnceLock<GeneralPurpose> = std::sync::OnceLock::new();
	let engine = ENGINE.get_or_init(|| {
		GeneralPurpose::new(
			&base64::alphabet::STANDARD,
			GeneralPurposeConfig::new()
				.with_decode_padding_mode(DecodePaddingMode::Indifferent)
				.with_decode_allow_trailing_bits(true),
		)
	});
	let sanitized: String = segment
		.chars()
		.filter(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '-' | '_'))
		.map(|c| match c {
			'-' => '+',
			'_' => '/',
			other => other,
		})
		.collect();
	let decoded = engine.decode(sanitized.as_bytes()).ok()?;
	let text = String::from_utf8(decoded).ok()?;
	serde_json::from_str(&text).ok()
}

fn decode_jwt(token: &str) -> Option<serde_json::Value> {
	let parts: Vec<&str> = token.split('.').collect();
	if parts.len() != 3 {
		return None;
	}
	decode_jwt_payload(parts[1])
}

async fn read_token_response(
	response: reqwest::Response,
	operation: &str,
) -> Result<OAuthToken, AuthError> {
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
			"OpenAI Codex token {operation} failed ({}): {body_or_status}",
			status.as_u16()
		)));
	}

	let json: serde_json::Value =
		serde_json::from_str(&text).map_err(|error| AuthError::Login(error.to_string()))?;
	let access = json
		.get("access_token")
		.and_then(|value| value.as_str())
		.filter(|value| !value.is_empty());
	let refresh = json
		.get("refresh_token")
		.and_then(|value| value.as_str())
		.filter(|value| !value.is_empty());
	let expires_in = json.get("expires_in").and_then(|value| value.as_f64());
	if access.is_none() || refresh.is_none() || expires_in.is_none() {
		return Err(AuthError::Login(format!(
			"OpenAI Codex token {operation} response missing fields: {json}"
		)));
	}
	Ok(OAuthToken {
		access: access.unwrap_or_default().to_string(),
		refresh: refresh.unwrap_or_default().to_string(),
		expires: super::now_ms() + (expires_in.unwrap_or_default() * 1000.0) as i64,
	})
}

async fn exchange_authorization_code(
	code: &str,
	verifier: &str,
	redirect_uri: &str,
	signal: tokio_util::sync::CancellationToken,
) -> Result<OAuthToken, AuthError> {
	let form = [
		("grant_type".to_string(), "authorization_code".to_string()),
		("client_id".to_string(), CLIENT_ID.to_string()),
		("code".to_string(), code.to_string()),
		("code_verifier".to_string(), verifier.to_string()),
		("redirect_uri".to_string(), redirect_uri.to_string()),
	];
	let response = super::send_request(
		reqwest::Method::POST,
		TOKEN_URL,
		&[("Content-Type", "application/x-www-form-urlencoded")],
		Some(crate::auth::http::encode_form(&form)),
		signal,
	)
	.await?;
	read_token_response(response, "exchange").await
}

async fn refresh_access_token(
	refresh_token: &str,
	signal: tokio_util::sync::CancellationToken,
) -> Result<OAuthToken, AuthError> {
	let form = [
		("grant_type".to_string(), "refresh_token".to_string()),
		("refresh_token".to_string(), refresh_token.to_string()),
		("client_id".to_string(), CLIENT_ID.to_string()),
	];
	let response = match super::send_request(
		reqwest::Method::POST,
		TOKEN_URL,
		&[("Content-Type", "application/x-www-form-urlencoded")],
		Some(crate::auth::http::encode_form(&form)),
		signal,
	)
	.await
	{
		Ok(response) => response,
		Err(error) => {
			return Err(AuthError::Login(format!(
				"OpenAI Codex token refresh error: {error}"
			)));
		}
	};
	read_token_response(response, "refresh").await
}

/// JS `Number(text)` for the device interval (strings are trimmed; "" is 0).
fn js_number(text: &str) -> Option<f64> {
	let trimmed = text.trim();
	if trimmed.is_empty() {
		return Some(0.0);
	}
	trimmed
		.parse::<f64>()
		.ok()
		.filter(|value| value.is_finite())
}

/// Pure half of `startOpenAICodexDeviceAuth`.
fn parse_device_code_response(
	json: Option<&serde_json::Value>,
) -> Result<DeviceAuthInfo, AuthError> {
	let interval_seconds = match json.and_then(|json| json.get("interval")) {
		Some(serde_json::Value::String(text)) => js_number(text),
		Some(value) => value.as_f64().filter(|value| value.is_finite()),
		None => None,
	};
	let device_auth_id = json
		.and_then(|json| json.get("device_auth_id"))
		.and_then(|value| value.as_str())
		.filter(|value| !value.is_empty());
	let user_code = json
		.and_then(|json| json.get("user_code"))
		.and_then(|value| value.as_str())
		.filter(|value| !value.is_empty());
	match (device_auth_id, user_code, interval_seconds) {
		(Some(device_auth_id), Some(user_code), Some(interval_seconds))
			if interval_seconds >= 0.0 =>
		{
			Ok(DeviceAuthInfo {
				device_auth_id: device_auth_id.to_string(),
				user_code: user_code.to_string(),
				interval_seconds,
			})
		}
		_ => Err(AuthError::Login(format!(
			"Invalid OpenAI Codex device code response: {}",
			serde_json::to_string(&json.cloned().unwrap_or(serde_json::Value::Null)).unwrap()
		))),
	}
}

async fn start_device_auth(
	signal: tokio_util::sync::CancellationToken,
) -> Result<DeviceAuthInfo, AuthError> {
	let response = super::send_request(
		reqwest::Method::POST,
		DEVICE_USER_CODE_URL,
		&[("Content-Type", "application/json")],
		Some(serde_json::json!({ "client_id": CLIENT_ID }).to_string()),
		signal,
	)
	.await?;
	let status = response.status().as_u16();
	let text = response
		.text()
		.await
		.map_err(|error| AuthError::Login(error.to_string()))?;
	if !(200..300).contains(&status) {
		if status == 404 {
			return Err(AuthError::Login(
				"OpenAI Codex device code login is not enabled for this server. Use browser login or verify the server URL."
					.to_string(),
			));
		}
		return Err(AuthError::Login(match text.is_empty() {
			true => format!("OpenAI Codex device code request failed with status {status}"),
			false => {
				format!("OpenAI Codex device code request failed with status {status}: {text}")
			}
		}));
	}
	let json: Option<serde_json::Value> = serde_json::from_str(&text).ok();
	parse_device_code_response(json.as_ref())
}

/// Extract the upstream error code: `error` may be a string or `{code}`.
fn device_auth_error_code(json: Option<&serde_json::Value>) -> Option<String> {
	match json.and_then(|json| json.get("error")) {
		Some(serde_json::Value::String(error)) => Some(error.clone()),
		Some(error @ serde_json::Value::Object(_)) => error
			.get("code")
			.and_then(|code| code.as_str())
			.map(str::to_string),
		_ => None,
	}
}

/// Classify one device-auth token poll (pure; the poller drives it).
fn device_auth_poll_result(
	ok: bool,
	status: u16,
	json: Option<&serde_json::Value>,
	body_text: &str,
) -> DevicePollResult<DeviceTokenSuccess> {
	if ok {
		let authorization_code = json
			.and_then(|json| json.get("authorization_code"))
			.and_then(|value| value.as_str())
			.filter(|value| !value.is_empty());
		let code_verifier = json
			.and_then(|json| json.get("code_verifier"))
			.and_then(|value| value.as_str())
			.filter(|value| !value.is_empty());
		return match (authorization_code, code_verifier) {
			(Some(authorization_code), Some(code_verifier)) => {
				DevicePollResult::Complete(DeviceTokenSuccess {
					authorization_code: authorization_code.to_string(),
					code_verifier: code_verifier.to_string(),
				})
			}
			_ => DevicePollResult::Failed {
				message: format!(
					"Invalid OpenAI Codex device auth token response: {}",
					serde_json::to_string(&json.cloned().unwrap_or(serde_json::Value::Null))
						.unwrap()
				),
			},
		};
	}

	if status == 403 || status == 404 {
		return DevicePollResult::Pending;
	}

	let error_code = device_auth_error_code(json);
	if error_code.as_deref() == Some("deviceauth_authorization_pending") {
		return DevicePollResult::Pending;
	}
	if error_code.as_deref() == Some("slow_down") {
		return DevicePollResult::SlowDown {
			interval_seconds: None,
		};
	}

	DevicePollResult::Failed {
		message: match body_text.is_empty() {
			true => format!("OpenAI Codex device auth failed with status {status}"),
			false => format!("OpenAI Codex device auth failed with status {status}: {body_text}"),
		},
	}
}

async fn poll_device_auth(
	device: &DeviceAuthInfo,
	signal: tokio_util::sync::CancellationToken,
) -> Result<DeviceTokenSuccess, AuthError> {
	let device_auth_id = device.device_auth_id.clone();
	let user_code = device.user_code.clone();
	let signal = signal.clone();
	poll_oauth_device_code_flow(
		Some(device.interval_seconds.max(0.0) as u64),
		Some(DEVICE_CODE_TIMEOUT_SECONDS),
		false,
		signal.clone(),
		move || {
			let device_auth_id = device_auth_id.clone();
			let user_code = user_code.clone();
			let signal = signal.clone();
			async move {
				let response = super::send_request(
					reqwest::Method::POST,
					DEVICE_TOKEN_URL,
					&[("Content-Type", "application/json")],
					Some(
						serde_json::json!({
							"device_auth_id": device_auth_id,
							"user_code": user_code
						})
						.to_string(),
					),
					signal,
				)
				.await?;
				let status = response.status();
				let ok = status.is_success();
				let text = response
					.text()
					.await
					.map_err(|error| AuthError::Login(error.to_string()))?;
				let json: Option<serde_json::Value> = serde_json::from_str(&text).ok();
				if ok && json.is_none() {
					// Upstream `response.json()` rejects unparseable bodies outright.
					return Err(AuthError::Login(
						"invalid JSON in device auth token response".to_string(),
					));
				}
				Ok(device_auth_poll_result(
					ok,
					status.as_u16(),
					json.as_ref(),
					&text,
				))
			}
		},
	)
	.await
}

fn create_authorization_flow(originator: &str) -> Result<(String, String, String), AuthError> {
	let (verifier, challenge) = generate_pkce()?;
	let state = create_state()?;

	let query = [
		("response_type", "code".to_string()),
		("client_id", CLIENT_ID.to_string()),
		("redirect_uri", REDIRECT_URI.to_string()),
		("scope", SCOPE.to_string()),
		("code_challenge", challenge),
		("code_challenge_method", "S256".to_string()),
		("state", state.clone()),
		("id_token_add_organizations", "true".to_string()),
		("codex_cli_simplified_flow", "true".to_string()),
		("originator", originator.to_string()),
	];
	let url = format!(
		"{AUTHORIZE_URL}?{}",
		crate::auth::http::encode_form(
			&query
				.iter()
				.map(|(k, v)| (k.to_string(), v.clone()))
				.collect::<Vec<_>>()
		)
	);
	Ok((verifier, state, url))
}

fn get_account_id(access_token: &str) -> Option<String> {
	let payload = decode_jwt(access_token)?;
	let account_id = payload
		.get(JWT_CLAIM_PATH)
		.and_then(|auth| auth.get("chatgpt_account_id"))
		.and_then(|value| value.as_str())
		.filter(|value| !value.is_empty())?;
	Some(account_id.to_string())
}

fn credentials_from_token(token: OAuthToken) -> Result<OAuthCredential, AuthError> {
	let Some(account_id) = get_account_id(&token.access) else {
		return Err(AuthError::Login(
			"Failed to extract accountId from token".to_string(),
		));
	};
	let mut extra = std::collections::BTreeMap::new();
	extra.insert(
		"accountId".to_string(),
		serde_json::Value::String(account_id),
	);
	Ok(OAuthCredential {
		access: token.access,
		refresh: token.refresh,
		expires: token.expires,
		extra,
	})
}

async fn exchange_authorization_code_for_credentials(
	code: &str,
	verifier: &str,
	redirect_uri: &str,
	signal: tokio_util::sync::CancellationToken,
) -> Result<OAuthCredential, AuthError> {
	credentials_from_token(exchange_authorization_code(code, verifier, redirect_uri, signal).await?)
}

async fn login_openai_codex_device_code(
	interaction: &dyn AuthInteraction,
) -> Result<OAuthCredential, AuthError> {
	let device = start_device_auth(interaction.signal()).await?;
	interaction.notify(AuthEvent::DeviceCode {
		user_code: device.user_code.clone(),
		verification_uri: DEVICE_VERIFICATION_URI.to_string(),
		open_browser_on_confirm: true,
		copy_code_on_confirm: true,
		interval_seconds: Some(device.interval_seconds.max(0.0) as u64),
		expires_in_seconds: Some(DEVICE_CODE_TIMEOUT_SECONDS),
	});
	let code = poll_device_auth(&device, interaction.signal()).await?;
	exchange_authorization_code_for_credentials(
		&code.authorization_code,
		&code.code_verifier,
		DEVICE_REDIRECT_URI,
		interaction.signal(),
	)
	.await
}

async fn login_openai_codex(
	interaction: &dyn AuthInteraction,
	options: &LoginOptions,
) -> Result<OAuthCredential, AuthError> {
	let (verifier, state, url) =
		create_authorization_flow(options.agent_name.as_deref().unwrap_or("pi"))?;
	// Port 1455 is shared with the Codex CLI; when it is taken, fall back to the pasted redirect URL.
	let callback = OAuthCallbackServer::start(CallbackServerOptions {
		provider_name: "OpenAI".to_string(),
		host: get_callback_host(),
		port: 1455,
		path: "/auth/callback".to_string(),
		redirect_host: None,
		state: Some(state.clone()),
		signal: interaction.signal(),
		timeout_ms: None,
	})
	.ok();

	interaction.notify(AuthEvent::AuthUrl {
		url,
		instructions: Some("A browser window should open. Complete login to finish.".to_string()),
	});

	let result: Result<OAuthCredential, AuthError> = async {
		let raced = wait_for_callback_or_manual_input(
			interaction,
			callback.as_ref(),
			"Complete login in your browser, or paste the authorization code / redirect URL here:",
			REDIRECT_URI,
		)
		.await?;
		let code = match raced {
			// Callback arm: the browser delivered the authorization code.
			Ok(code) => Some(code),
			// Manual arm: the user pasted a code or redirect URL.
			Err(input) => {
				let (parsed_code, parsed_state) = parse_authorization_input(&input);
				if parsed_state
					.as_deref()
					.is_some_and(|parsed_state| parsed_state != state)
				{
					return Err(AuthError::Login("State mismatch".to_string()));
				}
				parsed_code
			}
		};

		let Some(code) = code else {
			return Err(AuthError::Login("Missing authorization code".to_string()));
		};
		exchange_authorization_code_for_credentials(
			&code,
			&verifier,
			REDIRECT_URI,
			interaction.signal(),
		)
		.await
	}
	.await;
	if let Some(callback) = &callback {
		callback.close();
	}
	result
}

pub struct OpenAICodexOAuth;

impl OAuthFlow for OpenAICodexOAuth {
	fn name(&self) -> &str {
		"OpenAI (ChatGPT Plus/Pro)"
	}

	fn is_subscription(&self) -> bool {
		true
	}

	fn login<'a>(
		&'a self,
		interaction: &'a dyn AuthInteraction,
		options: &'a LoginOptions,
	) -> BoxFuture<'a, Result<OAuthCredential, AuthError>> {
		Box::pin(async move {
			let method = interaction
				.prompt(crate::auth::types::AuthPrompt {
					kind: AuthPromptKind::Select {
						message: "Select OpenAI Codex login method:".to_string(),
						options: vec![
							SelectOption {
								id: OPENAI_CODEX_BROWSER_LOGIN_METHOD.to_string(),
								label: "Browser login (default)".to_string(),
								description: None,
							},
							SelectOption {
								id: OPENAI_CODEX_DEVICE_CODE_LOGIN_METHOD.to_string(),
								label: "Device code login (headless)".to_string(),
								description: None,
							},
						],
					},
					signal: interaction.signal(),
				})
				.await?;

			if method == OPENAI_CODEX_DEVICE_CODE_LOGIN_METHOD {
				return login_openai_codex_device_code(interaction).await;
			}
			if method != OPENAI_CODEX_BROWSER_LOGIN_METHOD {
				return Err(AuthError::Login(format!(
					"Unknown OpenAI Codex login method: {method}"
				)));
			}

			login_openai_codex(interaction, options).await
		})
	}

	fn refresh(
		&self,
		credential: &OAuthCredential,
	) -> BoxFuture<'_, Result<OAuthCredential, AuthError>> {
		let refresh = credential.refresh.clone();
		Box::pin(async move {
			credentials_from_token(
				refresh_access_token(&refresh, tokio_util::sync::CancellationToken::new()).await?,
			)
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
	fn parse_authorization_input_covers_url_hash_query_and_bare_code() {
		assert_eq!(
			parse_authorization_input("https://localhost:1455/auth/callback?code=abc&state=xyz"),
			(Some("abc".to_string()), Some("xyz".to_string()))
		);
		// A URL without the parameters yields no code.
		assert_eq!(
			parse_authorization_input("https://localhost:1455/auth/callback"),
			(None, None)
		);
		assert_eq!(
			parse_authorization_input("code#state"),
			(Some("code".to_string()), Some("state".to_string()))
		);
		// Upstream `split("#", 2)` keeps only the second part.
		assert_eq!(
			parse_authorization_input("code#state#extra"),
			(Some("code".to_string()), Some("state".to_string()))
		);
		assert_eq!(
			parse_authorization_input("code=abc&state=xyz"),
			(Some("abc".to_string()), Some("xyz".to_string()))
		);
		// Upstream URLSearchParams keeps a leading "?" in the key, so no code.
		assert_eq!(parse_authorization_input("?code=abc"), (None, None));
		assert_eq!(
			parse_authorization_input("bare-code"),
			(Some("bare-code".to_string()), None)
		);
		assert_eq!(parse_authorization_input("   "), (None, None));
		// Percent-encoded query values are decoded.
		assert_eq!(
			parse_authorization_input("code=a%20b"),
			(Some("a b".to_string()), None)
		);
	}

	#[test]
	fn create_state_is_hex_and_unique() {
		let first = create_state().unwrap();
		let second = create_state().unwrap();
		assert_eq!(first.len(), 32);
		assert!(first.chars().all(|c| c.is_ascii_hexdigit()));
		assert_ne!(first, second);
	}

	#[test]
	fn decode_jwt_reads_base64url_payloads_and_rejects_garbage() {
		// {"https://api.openai.com/auth":{"chatgpt_account_id":"acc-1"}}
		let token = "eyJhbGciOiJub25lIn0.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjLTEifX0.sig";
		let payload = decode_jwt(token).unwrap();
		assert_eq!(
			payload["https://api.openai.com/auth"]["chatgpt_account_id"],
			json!("acc-1")
		);
		// Standard-alphabet payload decodes too ("[\"od\"]").
		assert_eq!(decode_jwt("a.WyJvZCJd.c").unwrap(), json!(["od"]));
		assert!(decode_jwt("not-a-jwt").is_none());
		assert!(decode_jwt("a.b.c.d").is_none());
		assert!(decode_jwt("a.%%%%.c").is_none());
		assert!(decode_jwt("a.eyJ9.c").is_none()); // "{"}" is not valid JSON
	}

	#[test]
	fn account_id_extracts_the_chatgpt_claim() {
		let token = "h.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjLTciLCJwbGFuIjoicHJvIn19.s";
		assert_eq!(get_account_id(token).as_deref(), Some("acc-7"));
		// Empty or missing account ids are null.
		let no_account = "h.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnt9LG90aGVyOjF9.s";
		assert_eq!(get_account_id(no_account), None);
		assert_eq!(get_account_id("garbage"), None);
	}

	#[test]
	fn credentials_require_the_account_id() {
		let error = credentials_from_token(OAuthToken {
			access: "a".to_string(),
			refresh: "r".to_string(),
			expires: 5,
		})
		.unwrap_err();
		assert_eq!(error.to_string(), "Failed to extract accountId from token");

		let access =
			"h.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWMifX0.s";
		let credential = credentials_from_token(OAuthToken {
			access: access.to_string(),
			refresh: "r".to_string(),
			expires: 5,
		})
		.unwrap();
		assert_eq!(
			credential
				.extra
				.get("accountId")
				.and_then(|value| value.as_str()),
			Some("ac")
		);
		assert_eq!(credential.access, access);
		assert_eq!(credential.refresh, "r");
		assert_eq!(credential.expires, 5);
	}

	#[test]
	fn device_code_response_parses_string_or_number_intervals() {
		let device = parse_device_code_response(Some(&json!({
			"device_auth_id": "da",
			"user_code": "WT7-JDQ",
			"interval": "5"
		})))
		.unwrap();
		assert_eq!(device.interval_seconds, 5.0);
		let device = parse_device_code_response(Some(&json!({
			"device_auth_id": "da",
			"user_code": "WT7-JDQ",
			"interval": "  2.5 "
		})))
		.unwrap();
		assert_eq!(device.interval_seconds, 2.5);
		let device = parse_device_code_response(Some(&json!({
			"device_auth_id": "da",
			"user_code": "WT7-JDQ",
			"interval": 0
		})))
		.unwrap();
		assert_eq!(device.interval_seconds, 0.0);

		// Negative, missing, or non-numeric intervals fail with the upstream copy.
		assert_eq!(
			parse_device_code_response(Some(
				&json!({"device_auth_id": "da", "user_code": "u", "interval": -1})
			))
			.unwrap_err()
			.to_string(),
			r#"Invalid OpenAI Codex device code response: {"device_auth_id":"da","user_code":"u","interval":-1}"#
		);
		assert_eq!(
			parse_device_code_response(Some(
				&json!({"device_auth_id": "da", "user_code": "u", "interval": "abc"})
			))
			.unwrap_err()
			.to_string(),
			r#"Invalid OpenAI Codex device code response: {"device_auth_id":"da","user_code":"u","interval":"abc"}"#
		);
		assert_eq!(
			parse_device_code_response(Some(&json!({"user_code": "u"})))
				.unwrap_err()
				.to_string(),
			r#"Invalid OpenAI Codex device code response: {"user_code":"u"}"#
		);
		assert_eq!(
			parse_device_code_response(None).unwrap_err().to_string(),
			"Invalid OpenAI Codex device code response: null"
		);
	}

	#[test]
	fn device_auth_poll_classifies_pending_slow_down_and_failures() {
		let complete = device_auth_poll_result(
			true,
			200,
			Some(&json!({"authorization_code": "ac", "code_verifier": "cv"})),
			"",
		);
		assert!(matches!(complete, DevicePollResult::Complete(_)));
		let invalid =
			device_auth_poll_result(true, 200, Some(&json!({"authorization_code": "ac"})), "");
		assert_eq!(
			match invalid {
				DevicePollResult::Failed { message } => message,
				_ => panic!("expected failure"),
			},
			r#"Invalid OpenAI Codex device auth token response: {"authorization_code":"ac"}"#
		);
		assert!(matches!(
			device_auth_poll_result(false, 403, None, ""),
			DevicePollResult::Pending
		));
		assert!(matches!(
			device_auth_poll_result(false, 404, None, ""),
			DevicePollResult::Pending
		));
		assert!(matches!(
			device_auth_poll_result(
				false,
				400,
				Some(&json!({"error": {"code": "deviceauth_authorization_pending"}})),
				""
			),
			DevicePollResult::Pending
		));
		assert!(matches!(
			device_auth_poll_result(
				false,
				429,
				Some(&json!({"error": {"code": "slow_down"}})),
				""
			),
			DevicePollResult::SlowDown {
				interval_seconds: None
			}
		));
		assert!(matches!(
			device_auth_poll_result(false, 429, Some(&json!({"error": "slow_down"})), ""),
			DevicePollResult::SlowDown {
				interval_seconds: None
			}
		));
		let failed = device_auth_poll_result(false, 500, None, "boom");
		assert_eq!(
			match failed {
				DevicePollResult::Failed { message } => message,
				_ => panic!("expected failure"),
			},
			"OpenAI Codex device auth failed with status 500: boom"
		);
		let failed = device_auth_poll_result(false, 400, Some(&json!({"error": "nope"})), "body");
		assert_eq!(
			match failed {
				DevicePollResult::Failed { message } => message,
				_ => panic!("expected failure"),
			},
			"OpenAI Codex device auth failed with status 400: body"
		);
	}

	#[test]
	fn authorize_url_parameters_carry_the_codex_extras() {
		let (_verifier, state, url) = create_authorization_flow("pi").unwrap();
		assert!(url.starts_with(&format!("{AUTHORIZE_URL}?")));
		let params = parse_query(url.split_once('?').unwrap().1);
		let find = |key: &str| {
			params
				.iter()
				.find(|(name, _)| name == key)
				.map(|(_, value)| value.clone())
		};
		assert_eq!(find("response_type").as_deref(), Some("code"));
		assert_eq!(find("client_id").as_deref(), Some(CLIENT_ID));
		assert_eq!(find("redirect_uri").as_deref(), Some(REDIRECT_URI));
		assert_eq!(find("scope").as_deref(), Some(SCOPE));
		assert_eq!(find("code_challenge_method").as_deref(), Some("S256"));
		assert_eq!(find("state"), Some(state.clone()));
		assert_eq!(find("id_token_add_organizations").as_deref(), Some("true"));
		assert_eq!(find("codex_cli_simplified_flow").as_deref(), Some("true"));
		assert_eq!(find("originator").as_deref(), Some("pi"));
		assert!(find("code_challenge").is_some());

		// A custom agent name becomes the originator (LoginOptions.agent_name).
		let (_verifier, _state, url) = create_authorization_flow("my-agent").unwrap();
		let params = parse_query(url.split_once('?').unwrap().1);
		assert_eq!(
			params
				.iter()
				.find(|(name, _)| name == "originator")
				.map(|(_, v)| v.as_str()),
			Some("my-agent")
		);
	}

	#[test]
	fn js_number_mirrors_the_upstream_interval_coercion() {
		assert_eq!(js_number("5"), Some(5.0));
		assert_eq!(js_number("  2.5 "), Some(2.5));
		assert_eq!(js_number(""), Some(0.0));
		assert_eq!(js_number("abc"), None);
		assert_eq!(js_number("Infinity"), None);
		assert_eq!(js_number("-0"), Some(0.0));
	}
}
