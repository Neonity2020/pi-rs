//! Meta Model API OAuth flow — port of `pi/packages/ai/src/auth/oauth/meta.ts`.
//!
//! RFC 8628 device authorization grant against https://auth.meta.com (JSON
//! responses). Meta splits identity from API access: the resulting identity
//! token is not accepted for inference, so it is exchanged for a Model API
//! key via the Muse Code key-mint endpoint (minted keys live about a day).
//! The identity token is stored as `refresh` and the minted key as `access`,
//! so the standard OAuth scheduler re-mints the key when it expires with no
//! bespoke renewal machinery.

use futures::future::BoxFuture;

use super::{now_ms, post_form_fields, send_request};
use crate::auth::device::{DevicePollResult, poll_oauth_device_code_flow};
use crate::auth::types::{
	AuthError, AuthEvent, AuthInteraction, LoginOptions, ModelAuth, OAuthCredential, OAuthFlow,
};

// Muse Code CLI client id.
const CLIENT_ID: &str = "1031625952748946";
const DEVICE_AUTHORIZATION_URL: &str = "https://auth.meta.com/oidc/device/authorization/";
const DEVICE_TOKEN_URL: &str = "https://auth.meta.com/oidc/device/token/";
const API_KEY_MINT_URL: &str = "https://api.meta.ai/muse-code/key";
const API_KEY_LIFETIME_MS: i64 = 24 * 60 * 60 * 1000;

const FORM_HEADERS: &[(&str, &str)] = &[
	("Content-Type", "application/x-www-form-urlencoded"),
	("Accept", "application/json"),
];

#[derive(Debug)]
struct DeviceAuthorization {
	device_code: String,
	user_code: String,
	verification_uri: String,
	interval_seconds: Option<u64>,
	expires_in_seconds: Option<u64>,
}

fn error_detail(json: Option<&serde_json::Value>) -> String {
	if let Some(json) = json {
		for key in ["error_description", "detail", "message", "error"] {
			if let Some(value) = json.get(key).and_then(|value| value.as_str())
				&& !value.trim().is_empty()
			{
				return format!(": {}", value.trim());
			}
		}
	}
	String::new()
}

/// The verification URI is opened in the user's browser; only http(s) URLs are trusted.
fn trusted_http_url(value: &serde_json::Value) -> Option<String> {
	let raw = value.as_str()?;
	if raw.is_empty() {
		return None;
	}
	let url = reqwest::Url::parse(raw).ok()?;
	if url.scheme() != "https" && url.scheme() != "http" {
		return None;
	}
	Some(url.to_string())
}

fn positive_number(value: Option<&serde_json::Value>) -> Option<u64> {
	value
		.and_then(|value| value.as_f64())
		.filter(|value| value.is_finite() && *value > 0.0)
		.map(|value| value as u64)
}

/// Pure half of `startDeviceAuthorization`: validate one device-authorization
/// response (already JSON-parsed; `None` for unparseable bodies).
fn parse_device_authorization(
	status: u16,
	json: Option<&serde_json::Value>,
) -> Result<DeviceAuthorization, AuthError> {
	if !(200..300).contains(&status) {
		return Err(AuthError::Login(format!(
			"Meta device authorization failed with status {status}{}",
			error_detail(json)
		)));
	}
	let device_code = json
		.and_then(|json| json.get("device_code"))
		.and_then(|value| value.as_str())
		.filter(|value| !value.is_empty());
	let user_code = json
		.and_then(|json| json.get("user_code"))
		.and_then(|value| value.as_str())
		.filter(|value| !value.is_empty());
	let verification_uri = json.and_then(|json| {
		trusted_http_url(&json["verification_uri_complete"])
			.or_else(|| trusted_http_url(&json["verification_uri"]))
	});
	if let (Some(device_code), Some(user_code), Some(verification_uri)) =
		(device_code, user_code, verification_uri)
	{
		return Ok(DeviceAuthorization {
			device_code: device_code.to_string(),
			user_code: user_code.to_string(),
			verification_uri,
			interval_seconds: positive_number(json.and_then(|json| json.get("interval"))),
			expires_in_seconds: positive_number(json.and_then(|json| json.get("expires_in"))),
		});
	}
	Err(AuthError::Login(format!(
		"Invalid Meta device authorization response: {}",
		serde_json::to_string(&json.cloned().unwrap_or(serde_json::Value::Null)).unwrap()
	)))
}

async fn start_device_authorization(
	signal: tokio_util::sync::CancellationToken,
) -> Result<DeviceAuthorization, AuthError> {
	let response = post_form_fields(
		DEVICE_AUTHORIZATION_URL,
		&[("client_id", CLIENT_ID)],
		FORM_HEADERS,
		signal,
	)
	.await?;
	let status = response.status().as_u16();
	let text = response
		.text()
		.await
		.map_err(|error| AuthError::Login(error.to_string()))?;
	let json: Option<serde_json::Value> = serde_json::from_str(&text).ok();
	parse_device_authorization(status, json.as_ref())
}

/// Classify one identity-token poll response (pure; the poller drives it).
fn poll_result(status: u16, json: Option<&serde_json::Value>) -> DevicePollResult<String> {
	let access_token = json
		.and_then(|json| json.get("access_token"))
		.and_then(|value| value.as_str())
		.filter(|value| !value.is_empty());
	if (200..300).contains(&status)
		&& let Some(access_token) = access_token
	{
		return DevicePollResult::Complete(access_token.to_string());
	}
	match json
		.and_then(|json| json.get("error"))
		.and_then(|value| value.as_str())
	{
		Some("authorization_pending") => DevicePollResult::Pending,
		Some("slow_down") => DevicePollResult::SlowDown {
			interval_seconds: positive_number(json.and_then(|json| json.get("interval"))),
		},
		Some("access_denied") => DevicePollResult::Failed {
			message: "Meta login was denied.".to_string(),
		},
		Some("expired_token") => DevicePollResult::Failed {
			message: "Meta device authorization expired. Please restart login.".to_string(),
		},
		_ => DevicePollResult::Failed {
			message: format!(
				"Meta device token request failed with status {status}{}",
				error_detail(json)
			),
		},
	}
}

async fn poll_for_identity_token(
	device: DeviceAuthorization,
	signal: tokio_util::sync::CancellationToken,
) -> Result<String, AuthError> {
	poll_oauth_device_code_flow(
		device.interval_seconds,
		device.expires_in_seconds,
		true,
		signal.clone(),
		move || {
			let device_code = device.device_code.clone();
			let signal = signal.clone();
			async move {
				let response = post_form_fields(
					DEVICE_TOKEN_URL,
					&[
						("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
						("device_code", &device_code),
						("client_id", CLIENT_ID),
					],
					FORM_HEADERS,
					signal,
				)
				.await?;
				let status = response.status().as_u16();
				let text = response
					.text()
					.await
					.map_err(|error| AuthError::Login(error.to_string()))?;
				let json: Option<serde_json::Value> = serde_json::from_str(&text).ok();
				Ok(poll_result(status, json.as_ref()))
			}
		},
	)
	.await
}

/// Pure half of `mintApiKey`: turn one key-mint response into a credential.
fn parse_mint_response(
	status: u16,
	json: Option<&serde_json::Value>,
) -> Result<OAuthCredential, AuthError> {
	if status == 401 || status == 403 {
		// Identity token is not renewable (see module header); only a fresh
		// device flow helps.
		return Err(AuthError::Login(format!(
			"Meta session expired (status {status}). Run `/login meta` to sign in again.{}",
			error_detail(json)
		)));
	}
	if !(200..300).contains(&status) {
		return Err(AuthError::Login(format!(
			"Meta API key mint failed with status {status}{}",
			error_detail(json)
		)));
	}
	let api_key = json
		.and_then(|json| json.get("api_key"))
		.and_then(|value| value.as_str())
		.filter(|value| !value.is_empty());
	match api_key {
		Some(api_key) => Ok(OAuthCredential {
			refresh: String::new(),
			access: api_key.to_string(),
			expires: now_ms() + API_KEY_LIFETIME_MS,
			extra: Default::default(),
		}),
		None => {
			let action_url = json
				.and_then(|json| json.get("action_url"))
				.and_then(trusted_http_url);
			Err(AuthError::Login(match action_url {
				Some(action_url) => {
					format!("Meta did not issue an API key. Complete setup at {action_url}")
				}
				None => "Meta did not issue an API key.".to_string(),
			}))
		}
	}
}

/// Exchange an identity token for a Model API key. Keys are valid for about a day.
async fn mint_api_key(
	identity_token: &str,
	signal: tokio_util::sync::CancellationToken,
) -> Result<OAuthCredential, AuthError> {
	let response = send_request(
		reqwest::Method::POST,
		API_KEY_MINT_URL,
		&[
			("Accept", "application/json"),
			("Authorization", &format!("Bearer {identity_token}")),
			("Content-Type", "application/json"),
			("x-api-version", "1.0.0"),
		],
		Some("{}".to_string()),
		signal,
	)
	.await?;
	let status = response.status().as_u16();
	let text = response
		.text()
		.await
		.map_err(|error| AuthError::Login(error.to_string()))?;
	let json: Option<serde_json::Value> = serde_json::from_str(&text).ok();
	let mut credential = parse_mint_response(status, json.as_ref())?;
	credential.refresh = identity_token.to_string();
	Ok(credential)
}

pub struct MetaOAuth;

impl OAuthFlow for MetaOAuth {
	fn name(&self) -> &str {
		"Meta (Muse subscription)"
	}

	fn is_subscription(&self) -> bool {
		true
	}

	fn login<'a>(
		&'a self,
		interaction: &'a dyn AuthInteraction,
		_options: &'a LoginOptions,
	) -> BoxFuture<'a, Result<OAuthCredential, AuthError>> {
		Box::pin(async move {
			let result: Result<OAuthCredential, AuthError> = async {
				let device = start_device_authorization(interaction.signal()).await?;
				interaction.notify(AuthEvent::DeviceCode {
					user_code: device.user_code.clone(),
					verification_uri: device.verification_uri.clone(),
					open_browser_on_confirm: true,
					copy_code_on_confirm: true,
					interval_seconds: device.interval_seconds,
					expires_in_seconds: device.expires_in_seconds,
				});
				let identity_token = poll_for_identity_token(device, interaction.signal()).await?;
				interaction.notify(AuthEvent::Progress {
					message: "Enabling Meta Model API access...".to_string(),
				});
				mint_api_key(&identity_token, interaction.signal()).await
			}
			.await;
			// An in-flight fetch rejects with a DOMException on abort; the login
			// UI matches on this message.
			match result {
				Ok(credential) => Ok(credential),
				Err(_) if interaction.signal().is_cancelled() => {
					Err(AuthError::Login("Login cancelled".to_string()))
				}
				Err(error) => Err(error),
			}
		})
	}

	fn refresh(
		&self,
		credential: &OAuthCredential,
	) -> BoxFuture<'_, Result<OAuthCredential, AuthError>> {
		let identity = credential.refresh.clone();
		Box::pin(async move {
			mint_api_key(&identity, tokio_util::sync::CancellationToken::new()).await
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
	fn error_detail_prefers_error_description_then_detail_then_message_then_error() {
		assert_eq!(
			error_detail(Some(&json!({"error_description": "  d ", "error": "e"}))),
			": d"
		);
		assert_eq!(
			error_detail(Some(&json!({"detail": "dt", "message": "m", "error": "e"}))),
			": dt"
		);
		assert_eq!(
			error_detail(Some(&json!({"message": "m", "error": "e"}))),
			": m"
		);
		assert_eq!(error_detail(Some(&json!({"error": "e"}))), ": e");
		// Blank strings are skipped.
		assert_eq!(error_detail(Some(&json!({"error": "   "}))), "");
		assert_eq!(error_detail(Some(&json!({"error": 5}))), "");
		assert_eq!(error_detail(None), "");
	}

	#[test]
	fn trusted_http_url_only_accepts_http_schemes() {
		assert_eq!(
			trusted_http_url(&json!("https://auth.meta.com/device")).as_deref(),
			Some("https://auth.meta.com/device")
		);
		assert_eq!(
			trusted_http_url(&json!("http://auth.meta.com/device")).as_deref(),
			Some("http://auth.meta.com/device")
		);
		assert_eq!(trusted_http_url(&json!("ftp://auth.meta.com/device")), None);
		assert_eq!(trusted_http_url(&json!("not a url")), None);
		assert_eq!(trusted_http_url(&json!("")), None);
		assert_eq!(trusted_http_url(&json!(5)), None);
	}

	#[test]
	fn device_authorization_prefers_the_complete_uri_and_defaults_timings() {
		let device = parse_device_authorization(
			200,
			Some(&json!({
				"device_code": "dc",
				"user_code": "ABCD-1234",
				"verification_uri": "https://auth.meta.com/device",
				"verification_uri_complete": "https://auth.meta.com/device?code=ABCD-1234",
				"interval": 4,
				"expires_in": 800
			})),
		)
		.unwrap();
		assert_eq!(
			device.verification_uri,
			"https://auth.meta.com/device?code=ABCD-1234"
		);
		assert_eq!(device.interval_seconds, Some(4));
		assert_eq!(device.expires_in_seconds, Some(800));

		// Falls back to verification_uri and drops non-positive timings.
		let device = parse_device_authorization(
			200,
			Some(&json!({
				"device_code": "dc",
				"user_code": "ABCD-1234",
				"verification_uri": "http://auth.meta.com/device",
				"interval": 0,
				"expires_in": -1
			})),
		)
		.unwrap();
		assert_eq!(device.verification_uri, "http://auth.meta.com/device");
		assert_eq!(device.interval_seconds, None);
		assert_eq!(device.expires_in_seconds, None);
	}

	#[test]
	fn device_authorization_error_messages_match_upstream() {
		let error = parse_device_authorization(503, Some(&json!({"message": "down"}))).unwrap_err();
		assert_eq!(
			error.to_string(),
			"Meta device authorization failed with status 503: down"
		);
		let error =
			parse_device_authorization(200, Some(&json!({"device_code": "dc"}))).unwrap_err();
		assert_eq!(
			error.to_string(),
			r#"Invalid Meta device authorization response: {"device_code":"dc"}"#
		);
		// Unparseable body → JSON null.
		let error = parse_device_authorization(200, None).unwrap_err();
		assert_eq!(
			error.to_string(),
			"Invalid Meta device authorization response: null"
		);
	}

	#[test]
	fn poll_result_classifies_identity_token_polls() {
		assert!(matches!(
			poll_result(200, Some(&json!({"access_token": "tok"}))),
			DevicePollResult::Complete(value) if value == "tok"
		));
		assert!(matches!(
			poll_result(200, Some(&json!({}))),
			DevicePollResult::Failed { .. }
		));
		assert!(matches!(
			poll_result(400, Some(&json!({"error": "authorization_pending"}))),
			DevicePollResult::Pending
		));
		assert!(matches!(
			poll_result(400, Some(&json!({"error": "slow_down", "interval": 6}))),
			DevicePollResult::SlowDown {
				interval_seconds: Some(6)
			}
		));
		let denied = poll_result(403, Some(&json!({"error": "access_denied"})));
		assert_eq!(
			match denied {
				DevicePollResult::Failed { message } => message,
				_ => panic!("expected failure"),
			},
			"Meta login was denied."
		);
		let expired = poll_result(400, Some(&json!({"error": "expired_token"})));
		assert_eq!(
			match expired {
				DevicePollResult::Failed { message } => message,
				_ => panic!("expected failure"),
			},
			"Meta device authorization expired. Please restart login."
		);
		let other = poll_result(500, Some(&json!({"error": "boom"})));
		assert_eq!(
			match other {
				DevicePollResult::Failed { message } => message,
				_ => panic!("expected failure"),
			},
			"Meta device token request failed with status 500: boom"
		);
	}

	#[test]
	fn mint_response_messages_match_upstream() {
		let error = parse_mint_response(401, Some(&json!({"detail": "stale"}))).unwrap_err();
		assert_eq!(
			error.to_string(),
			"Meta session expired (status 401). Run `/login meta` to sign in again.: stale"
		);
		let error = parse_mint_response(500, None).unwrap_err();
		assert_eq!(
			error.to_string(),
			"Meta API key mint failed with status 500"
		);
		let error = parse_mint_response(200, Some(&json!({"action_url": "https://meta.ai/setup"})))
			.unwrap_err();
		assert_eq!(
			error.to_string(),
			"Meta did not issue an API key. Complete setup at https://meta.ai/setup"
		);
		let error = parse_mint_response(200, Some(&json!({"api_key": 3}))).unwrap_err();
		assert_eq!(error.to_string(), "Meta did not issue an API key.");

		let credential = parse_mint_response(200, Some(&json!({"api_key": "key"}))).unwrap();
		assert_eq!(credential.access, "key");
		assert_eq!(credential.refresh, "");
		assert!(credential.expires >= now_ms() + API_KEY_LIFETIME_MS - 1000);
	}
}
