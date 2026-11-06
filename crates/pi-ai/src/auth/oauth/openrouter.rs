//! OpenRouter OAuth PKCE flow — port of
//! `pi/packages/ai/src/auth/oauth/openrouter.ts`.
//!
//! OpenRouter exchanges an authorization code for a permanent, user-controlled
//! API key rather than an expiring access/refresh token pair. The callback is
//! handled by a one-shot loopback server on an ephemeral port, raced against a
//! manual prompt so remote/headless sessions can paste the redirect URL when
//! the browser cannot reach the loopback server.

use futures::future::BoxFuture;

use super::wait_for_callback_or_manual_input;
use crate::auth::callback::{
	CallbackServerOptions, OAuthCallbackServer, oauth_error_html, oauth_success_html,
};
use crate::auth::pkce::{generate_pkce, getrandom_fill};
use crate::auth::types::{
	AuthError, AuthEvent, AuthInteraction, LoginOptions, ModelAuth, OAuthCredential, OAuthFlow,
};

const AUTHORIZE_URL: &str = "https://openrouter.ai/auth";
const TOKEN_URL: &str = "https://openrouter.ai/api/v1/auth/keys";
const LOGIN_TIMEOUT_MS: u64 = 5 * 60 * 1000;
// The shared request helper carries the same 30s wall clock.
const TOKEN_EXCHANGE_TIMEOUT_NOTE: &str = "OpenRouter OAuth token exchange timed out";

const CALLBACK_HOST_ENV: &str = "PI_OAUTH_CALLBACK_HOST";

fn get_callback_host() -> String {
	std::env::var(CALLBACK_HOST_ENV)
		.ok()
		.filter(|value| !value.is_empty())
		.unwrap_or_else(|| "127.0.0.1".to_string())
}

fn parse_authorization_input(input: &str) -> Option<String> {
	let value = input.trim();
	if value.is_empty() {
		return None;
	}

	if let Ok(url) = reqwest::Url::parse(value) {
		return url
			.query_pairs()
			.find(|(name, _)| name == "code")
			.map(|(_, value)| value.into_owned())
			.filter(|code| !code.is_empty());
	}

	if value.contains("code=") {
		return crate::auth::callback::parse_query(value)
			.into_iter()
			.find(|(name, _)| name == "code")
			.map(|(_, value)| value)
			.filter(|code| !code.is_empty());
	}

	Some(value.to_string())
}

fn error_detail(body: &serde_json::Value) -> Option<String> {
	if let Some(detail) = body
		.get("error_description")
		.and_then(|value| value.as_str())
	{
		return Some(detail.to_string());
	}
	if let Some(detail) = body.get("message").and_then(|value| value.as_str()) {
		return Some(detail.to_string());
	}
	if let Some(detail) = body.get("error").and_then(|value| value.as_str()) {
		return Some(detail.to_string());
	}
	if let Some(message) = body
		.get("error")
		.filter(|error| error.is_object())
		.and_then(|error| error.get("message"))
		.and_then(|value| value.as_str())
	{
		return Some(message.to_string());
	}
	None
}

/// Pure half of `exchangeAuthorizationCode`: classify one token-exchange
/// response (`text` is the raw body).
fn exchange_outcome(status: u16, ok: bool, text: &str) -> Result<OAuthCredential, String> {
	let mut body = serde_json::json!({});
	match serde_json::from_str::<serde_json::Value>(text) {
		Ok(parsed) if parsed.is_object() => body = parsed,
		Ok(_) => {}
		Err(_) => {
			if ok {
				return Err("OpenRouter OAuth returned invalid JSON".to_string());
			}
		}
	}

	if !ok {
		let detail = error_detail(&body);
		return Err(match detail {
			Some(detail) => {
				format!("OpenRouter OAuth key exchange failed (HTTP {status}): {detail}")
			}
			None => format!("OpenRouter OAuth key exchange failed (HTTP {status})"),
		});
	}

	let key = body
		.get("key")
		.and_then(|value| value.as_str())
		.filter(|key| !key.is_empty());
	match key {
		Some(key) => Ok(OAuthCredential {
			access: key.to_string(),
			refresh: String::new(),
			// `Number.MAX_SAFE_INTEGER`.
			expires: 9007199254740991,
			extra: Default::default(),
		}),
		None => Err("OpenRouter OAuth response carries no \"key\"".to_string()),
	}
}

async fn exchange_authorization_code(
	code: &str,
	verifier: &str,
	signal: tokio_util::sync::CancellationToken,
) -> Result<OAuthCredential, AuthError> {
	if signal.is_cancelled() {
		return Err(AuthError::Login("Login cancelled".to_string()));
	}
	let body = serde_json::json!({
		"code": code,
		"code_verifier": verifier,
		"code_challenge_method": "S256"
	});
	let response = match super::send_request(
		reqwest::Method::POST,
		TOKEN_URL,
		&[
			("accept", "application/json"),
			("content-type", "application/json"),
		],
		Some(body.to_string()),
		signal.clone(),
	)
	.await
	{
		Ok(response) => response,
		Err(error) => {
			if signal.is_cancelled() {
				return Err(AuthError::Login("Login cancelled".to_string()));
			}
			// The shared helper's 30s wall clock is the token-exchange timeout.
			if error.to_string().contains("timed out") {
				return Err(AuthError::Login(TOKEN_EXCHANGE_TIMEOUT_NOTE.to_string()));
			}
			return Err(error);
		}
	};
	let status = response.status();
	let text = response
		.text()
		.await
		.map_err(|error| AuthError::Login(error.to_string()))?;
	exchange_outcome(status.as_u16(), status.is_success(), &text).map_err(AuthError::Login)
}

fn random_uuid_v4() -> Result<String, AuthError> {
	let mut bytes = [0u8; 16];
	getrandom_fill(&mut bytes)?;
	bytes[6] = (bytes[6] & 0x0f) | 0x40;
	bytes[8] = (bytes[8] & 0x3f) | 0x80;
	let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
	Ok(format!(
		"{}-{}-{}-{}-{}",
		&hex[0..8],
		&hex[8..12],
		&hex[12..16],
		&hex[16..20],
		&hex[20..32]
	))
}

async fn login_openrouter(interaction: &dyn AuthInteraction) -> Result<OAuthCredential, AuthError> {
	let (verifier, challenge) = generate_pkce()?;
	let path = format!("/oauth/callback/{}", random_uuid_v4()?);
	// OpenRouter sends no `state`; the random path keeps stray requests from completing the sign-in.
	let callback = OAuthCallbackServer::start(CallbackServerOptions {
		provider_name: "OpenRouter".to_string(),
		host: get_callback_host(),
		port: 0,
		path,
		redirect_host: None,
		state: None,
		signal: interaction.signal(),
		timeout_ms: Some(LOGIN_TIMEOUT_MS),
	})?;
	let redirect_uri = callback.redirect_uri.clone();

	let result: Result<OAuthCredential, AuthError> = async {
		let query = [
			("callback_url", redirect_uri.clone()),
			("code_challenge", challenge),
			("code_challenge_method", "S256".to_string()),
		];
		let authorize_url = format!(
			"{AUTHORIZE_URL}?{}",
			crate::auth::http::encode_form(&query.iter().map(|(k, v)| (k.to_string(), v.clone())).collect::<Vec<_>>())
		);

		interaction.notify(AuthEvent::Progress {
			message: format!("Listening for OpenRouter OAuth callback on {redirect_uri}"),
		});
		interaction.notify(AuthEvent::AuthUrl {
			url: authorize_url,
			instructions: Some(
				"Complete sign-in in your browser. If the browser is on another machine, paste the final redirect URL here."
					.to_string(),
			),
		});

		let raced = wait_for_callback_or_manual_input(
			interaction,
			Some(&callback),
			"Complete sign-in in your browser, or paste the authorization code / redirect URL here:",
			&redirect_uri,
		)
		.await?;
		match raced {
			// Callback arm: like upstream, the exchange result decides the page
			// the browser sees on the parked connection.
			Ok(code) => {
				let outcome = exchange_authorization_code(&code, &verifier, interaction.signal()).await;
				match &outcome {
					Ok(_) => callback.complete_with(
						true,
						oauth_success_html("Signed in to OpenRouter. You may now close this page."),
					),
					Err(error) => callback.complete_with(
						false,
						oauth_error_html("OpenRouter sign-in failed.", Some(&error.to_string())),
					),
				}
				outcome
			}
			Err(input) => {
				let Some(code) = parse_authorization_input(&input) else {
					return Err(AuthError::Login("Missing authorization code".to_string()));
				};
				interaction.notify(AuthEvent::Progress {
					message: "Exchanging authorization code for an API key...".to_string(),
				});
				exchange_authorization_code(&code, &verifier, interaction.signal()).await
			}
		}
	}
	.await;
	callback.close();
	result
}

pub struct OpenRouterOAuth;

impl OAuthFlow for OpenRouterOAuth {
	fn name(&self) -> &str {
		"OpenRouter OAuth"
	}

	fn login<'a>(
		&'a self,
		interaction: &'a dyn AuthInteraction,
		_options: &'a LoginOptions,
	) -> BoxFuture<'a, Result<OAuthCredential, AuthError>> {
		Box::pin(async move { login_openrouter(interaction).await })
	}

	fn refresh(
		&self,
		credential: &OAuthCredential,
	) -> BoxFuture<'_, Result<OAuthCredential, AuthError>> {
		let credential = credential.clone();
		Box::pin(async move { Ok(credential) })
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
	fn parse_authorization_input_reads_urls_query_and_bare_values() {
		assert_eq!(
			parse_authorization_input("https://openrouter.ai/oauth/callback/xyz?code=abc"),
			Some("abc".to_string())
		);
		// A URL without a code parameter stays None (no fallback to raw value).
		assert_eq!(
			parse_authorization_input("https://openrouter.ai/auth"),
			None
		);
		assert_eq!(
			parse_authorization_input("code=abc&other=1"),
			Some("abc".to_string())
		);
		assert_eq!(
			parse_authorization_input("plain-code"),
			Some("plain-code".to_string())
		);
		// Empty code parameter counts as missing, like upstream's falsy check.
		assert_eq!(parse_authorization_input("code="), None);
		assert_eq!(parse_authorization_input("   "), None);
	}

	#[test]
	fn error_detail_prefers_description_then_message_then_error() {
		assert_eq!(
			error_detail(&json!({"error_description": "d", "message": "m", "error": "e"})),
			Some("d".to_string())
		);
		assert_eq!(
			error_detail(&json!({"message": "m", "error": "e"})),
			Some("m".to_string())
		);
		assert_eq!(error_detail(&json!({"error": "e"})), Some("e".to_string()));
		assert_eq!(
			error_detail(&json!({"error": {"message": "nested"}})),
			Some("nested".to_string())
		);
		// Arrays are not treated as error objects.
		assert_eq!(error_detail(&json!({"error": ["nested"]})), None);
		assert_eq!(error_detail(&json!({})), None);
	}

	#[test]
	fn exchange_outcome_maps_key_and_error_shapes() {
		let credential = exchange_outcome(200, true, r#"{"key":"sk-or-v1-abc"}"#).unwrap();
		assert_eq!(credential.access, "sk-or-v1-abc");
		assert_eq!(credential.refresh, "");
		assert_eq!(credential.expires, 9007199254740991);

		assert_eq!(
			exchange_outcome(200, true, r#"{"nope":1}"#).unwrap_err(),
			"OpenRouter OAuth response carries no \"key\""
		);
		assert_eq!(
			exchange_outcome(200, true, "not json").unwrap_err(),
			"OpenRouter OAuth returned invalid JSON"
		);
		// A JSON array parses fine but carries no key (upstream keeps `{}`).
		assert_eq!(
			exchange_outcome(200, true, "[1]").unwrap_err(),
			"OpenRouter OAuth response carries no \"key\""
		);
		assert_eq!(
			exchange_outcome(401, false, r#"{"error":"invalid_grant"}"#).unwrap_err(),
			"OpenRouter OAuth key exchange failed (HTTP 401): invalid_grant"
		);
		assert_eq!(
			exchange_outcome(500, false, r#"{"error":{"message":"boom"}}"#).unwrap_err(),
			"OpenRouter OAuth key exchange failed (HTTP 500): boom"
		);
		assert_eq!(
			exchange_outcome(502, false, "bad gateway").unwrap_err(),
			"OpenRouter OAuth key exchange failed (HTTP 502)"
		);
	}

	#[test]
	fn uuid_v4_has_the_right_shape() {
		for _ in 0..16 {
			let uuid = random_uuid_v4().unwrap();
			assert_eq!(uuid.len(), 36);
			let bytes = uuid.as_bytes();
			for (index, byte) in bytes.iter().enumerate() {
				match index {
					8 | 13 | 18 | 23 => assert_eq!(*byte, b'-'),
					_ => assert!(byte.is_ascii_hexdigit()),
				}
			}
			// Version nibble is 4 and the variant nibble is 8/9/a/b.
			assert_eq!(uuid.as_bytes()[14], b'4');
			assert!(matches!(uuid.as_bytes()[19], b'8' | b'9' | b'a' | b'b'));
			assert_ne!(uuid, random_uuid_v4().unwrap());
		}
	}
}
