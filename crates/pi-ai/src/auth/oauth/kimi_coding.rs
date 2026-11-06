//! Kimi Code (subscription) OAuth flow — port of
//! `pi/packages/ai/src/auth/oauth/kimi-coding.ts`.
//!
//! RFC 8628 device authorization grant against https://auth.kimi.com with JSON
//! responses. The access token authenticates requests to
//! https://api.kimi.com/coding as an `Authorization: Bearer` header.

use futures::future::BoxFuture;

use super::{now_ms, post_form_fields};
use crate::auth::device::{DevicePollResult, poll_oauth_device_code_flow};
use crate::auth::types::{
	AuthError, AuthEvent, AuthInteraction, LoginOptions, ModelAuth, OAuthCredential, OAuthFlow,
};

const CLIENT_ID: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";
const DEFAULT_OAUTH_HOST: &str = "https://auth.kimi.com";
const DEVICE_CODE_TIMEOUT_SECONDS: u64 = 15 * 60;
const DEFAULT_POLL_INTERVAL_SECONDS: u64 = 5;
const REFRESH_MAX_RETRIES: u32 = 3;

const FORM_HEADERS: &[(&str, &str)] = &[
	("Content-Type", "application/x-www-form-urlencoded"),
	("Accept", "application/json"),
];

#[derive(Debug)]
struct DeviceAuthorization {
	device_code: String,
	user_code: String,
	verification_uri_complete: String,
	interval_seconds: u64,
	expires_in_seconds: u64,
}

#[derive(Debug)]
struct TokenResponse {
	access: String,
	refresh: String,
	expires: i64,
}

fn get_oauth_host() -> String {
	let override_value = std::env::var("KIMI_CODE_OAUTH_HOST")
		.ok()
		.filter(|value| !value.is_empty())
		.or_else(|| {
			std::env::var("KIMI_OAUTH_HOST")
				.ok()
				.filter(|value| !value.is_empty())
		});
	oauth_host_from(override_value.as_deref())
}

fn oauth_host_from(override_value: Option<&str>) -> String {
	match override_value {
		Some(value) if !value.is_empty() => value.trim_end_matches('/').to_string(),
		_ => DEFAULT_OAUTH_HOST.to_string(),
	}
}

/// The verification URI is opened in the user's browser; only http(s) URLs are trusted.
fn trusted_http_url(value: Option<&serde_json::Value>) -> Option<String> {
	let raw = value?.as_str()?;
	if raw.is_empty() {
		return None;
	}
	let url = reqwest::Url::parse(raw).ok()?;
	if url.scheme() != "https" && url.scheme() != "http" {
		return None;
	}
	Some(url.to_string())
}

fn positive_finite(value: Option<&serde_json::Value>) -> Option<f64> {
	value
		.and_then(|value| value.as_f64())
		.filter(|value| value.is_finite() && *value > 0.0)
}

/// Pure half of `startDeviceAuthorization` (validation + error copy).
fn parse_device_authorization(
	status: u16,
	text: &str,
	json: Option<&serde_json::Value>,
) -> Result<DeviceAuthorization, AuthError> {
	if !(200..300).contains(&status) {
		return Err(AuthError::Login(match text.is_empty() {
			true => format!("Kimi Code device authorization failed with status {status}"),
			false => format!("Kimi Code device authorization failed with status {status}: {text}"),
		}));
	}
	let device_code = json
		.and_then(|json| json.get("device_code"))
		.and_then(|value| value.as_str());
	let user_code = json
		.and_then(|json| json.get("user_code"))
		.and_then(|value| value.as_str());
	let verification_uri = json
		.and_then(|json| json.get("verification_uri"))
		.and_then(|value| value.as_str());
	let verification_uri_complete = json
		.and_then(|json| json.get("verification_uri_complete"))
		.and_then(|value| value.as_str());
	let valid = [
		device_code,
		user_code,
		verification_uri,
		verification_uri_complete,
	]
	.into_iter()
	.all(|value| value.is_some_and(|value| !value.is_empty()))
		&& trusted_http_url(json.and_then(|json| json.get("verification_uri_complete"))).is_some()
		&& trusted_http_url(json.and_then(|json| json.get("verification_uri"))).is_some();
	if !valid {
		return Err(AuthError::Login(format!(
			"Invalid Kimi Code device authorization response: {}",
			serde_json::to_string(&json.cloned().unwrap_or(serde_json::Value::Null)).unwrap()
		)));
	}
	Ok(DeviceAuthorization {
		device_code: device_code.unwrap_or_default().to_string(),
		user_code: user_code.unwrap_or_default().to_string(),
		verification_uri_complete: verification_uri_complete.unwrap_or_default().to_string(),
		interval_seconds: positive_finite(json.and_then(|json| json.get("interval")))
			.map(|value| value as u64)
			.unwrap_or(DEFAULT_POLL_INTERVAL_SECONDS),
		expires_in_seconds: positive_finite(json.and_then(|json| json.get("expires_in")))
			.map(|value| value as u64)
			.unwrap_or(DEVICE_CODE_TIMEOUT_SECONDS),
	})
}

async fn start_device_authorization(
	oauth_host: &str,
	signal: tokio_util::sync::CancellationToken,
) -> Result<DeviceAuthorization, AuthError> {
	let response = post_form_fields(
		&format!("{oauth_host}/api/oauth/device_authorization"),
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
	parse_device_authorization(status, &text, json.as_ref())
}

/// Pure half of `parseTokenResponse` (validation + error copy).
fn parse_token_response(
	json: Option<&serde_json::Value>,
	operation: &str,
) -> Result<TokenResponse, AuthError> {
	let access_token = json
		.and_then(|json| json.get("access_token"))
		.and_then(|value| value.as_str())
		.filter(|value| !value.is_empty());
	let refresh_token = json
		.and_then(|json| json.get("refresh_token"))
		.and_then(|value| value.as_str())
		.filter(|value| !value.is_empty());
	let expires_in = positive_finite(json.and_then(|json| json.get("expires_in")));
	match (access_token, refresh_token, expires_in) {
		(Some(access_token), Some(refresh_token), Some(expires_in)) => Ok(TokenResponse {
			access: access_token.to_string(),
			refresh: refresh_token.to_string(),
			expires: now_ms() + (expires_in * 1000.0) as i64,
		}),
		_ => Err(AuthError::Login(format!(
			"Kimi Code token {operation} response missing fields: {}",
			serde_json::to_string(&json.cloned().unwrap_or(serde_json::Value::Null)).unwrap()
		))),
	}
}

/// Classify one device-token poll response (pure; the poller drives it).
fn poll_result(
	status: u16,
	text: &str,
	json: Option<&serde_json::Value>,
) -> DevicePollResult<TokenResponse> {
	if status >= 500 {
		return DevicePollResult::Failed {
			message: match text.is_empty() {
				true => format!("Kimi Code device token request failed with status {status}"),
				false => {
					format!("Kimi Code device token request failed with status {status}: {text}")
				}
			},
		};
	}
	if (200..300).contains(&status)
		&& json
			.and_then(|json| json.get("access_token"))
			.and_then(|value| value.as_str())
			.is_some()
	{
		return match parse_token_response(json, "poll") {
			Ok(token) => DevicePollResult::Complete(token),
			Err(error) => DevicePollResult::Failed {
				message: error.to_string(),
			},
		};
	}
	let error = json.and_then(|json| json.get("error"));
	let description = match json
		.and_then(|json| json.get("error_description"))
		.and_then(|value| value.as_str())
	{
		Some(description) => format!(": {description}"),
		None => String::new(),
	};
	match error.and_then(|value| value.as_str()) {
		Some("authorization_pending") => DevicePollResult::Pending,
		Some("slow_down") => DevicePollResult::SlowDown {
			interval_seconds: positive_finite(json.and_then(|json| json.get("interval")))
				.filter(|value| *value > 0.0)
				.map(|value| value as u64),
		},
		Some("expired_token") => DevicePollResult::Failed {
			message: "Kimi Code device authorization expired. Please restart login.".to_string(),
		},
		Some("access_denied") => DevicePollResult::Failed {
			message: "Kimi Code login was denied.".to_string(),
		},
		_ => DevicePollResult::Failed {
			message: match error.and_then(|value| value.as_str()) {
				Some(error) => format!(
					"Kimi Code device token request failed (status {status}): {error}{description}"
				),
				None => format!("Kimi Code device token request failed (status {status})"),
			},
		},
	}
}

async fn poll_for_token(
	oauth_host: &str,
	device: &DeviceAuthorization,
	signal: tokio_util::sync::CancellationToken,
) -> Result<TokenResponse, AuthError> {
	let device_code = device.device_code.clone();
	let oauth_host = oauth_host.to_string();
	let signal = signal.clone();
	poll_oauth_device_code_flow(
		Some(device.interval_seconds),
		Some(device.expires_in_seconds),
		true,
		signal.clone(),
		move || {
			let device_code = device_code.clone();
			let oauth_host = oauth_host.clone();
			let signal = signal.clone();
			async move {
				let response = post_form_fields(
					&format!("{oauth_host}/api/oauth/token"),
					&[
						("client_id", CLIENT_ID),
						("device_code", &device_code),
						("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
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
				Ok(poll_result(status, &text, json.as_ref()))
			}
		},
	)
	.await
}

/// One refresh attempt outcome (pure; `refresh_token` drives the loop).
enum RefreshStep {
	Done(TokenResponse),
	Unauthorized(String),
	Retry(String),
	Fatal(String),
}

fn refresh_step(status: u16, json: Option<&serde_json::Value>) -> RefreshStep {
	if (200..300).contains(&status) {
		return match parse_token_response(json, "refresh") {
			Ok(token) => RefreshStep::Done(token),
			Err(error) => RefreshStep::Fatal(error.to_string()),
		};
	}
	let description = match json
		.and_then(|json| json.get("error_description"))
		.and_then(|value| value.as_str())
	{
		Some(description) => format!(": {description}"),
		None => String::new(),
	};
	// Unauthorized: the stored credential is dead; Models clears it and prompts re-login.
	if status == 401
		|| status == 403
		|| json
			.and_then(|json| json.get("error"))
			.and_then(|value| value.as_str())
			== Some("invalid_grant")
	{
		return RefreshStep::Unauthorized(format!(
			"Kimi Code token refresh unauthorized (status {status}){description}"
		));
	}
	if status == 429 || status >= 500 {
		return RefreshStep::Retry(format!(
			"Kimi Code token refresh failed with status {status}"
		));
	}
	let text = serde_json::to_string(&json.cloned().unwrap_or(serde_json::Value::Null)).unwrap();
	RefreshStep::Fatal(format!(
		"Kimi Code token refresh failed with status {status}: {text}"
	))
}

async fn refresh_token(
	oauth_host: &str,
	refresh_token_value: &str,
	signal: tokio_util::sync::CancellationToken,
) -> Result<TokenResponse, AuthError> {
	let mut last_error: Option<AuthError> = None;
	for attempt in 0..=REFRESH_MAX_RETRIES {
		if attempt > 0 {
			let delay = std::time::Duration::from_millis(1000 * 2u64.pow(attempt - 1));
			tokio::select! {
				() = tokio::time::sleep(delay) => {}
				() = signal.cancelled() => {
					return Err(AuthError::Login("Kimi Code token refresh aborted".to_string()));
				}
			}
		}
		if signal.is_cancelled() {
			return Err(AuthError::Login(
				"Kimi Code token refresh aborted".to_string(),
			));
		}

		let response = post_form_fields(
			&format!("{oauth_host}/api/oauth/token"),
			&[
				("client_id", CLIENT_ID),
				("grant_type", "refresh_token"),
				("refresh_token", refresh_token_value),
			],
			FORM_HEADERS,
			signal.clone(),
		)
		.await;
		let response = match response {
			Ok(response) => response,
			Err(error) => {
				if signal.is_cancelled() {
					return Err(error);
				}
				last_error = Some(error);
				continue;
			}
		};

		let status = response.status().as_u16();
		let text = response
			.text()
			.await
			.map_err(|error| AuthError::Login(error.to_string()))?;
		let json: Option<serde_json::Value> = serde_json::from_str(&text).ok();
		match refresh_step(status, json.as_ref()) {
			RefreshStep::Done(token) => return Ok(token),
			RefreshStep::Unauthorized(message) => return Err(AuthError::Login(message)),
			RefreshStep::Retry(message) if attempt < REFRESH_MAX_RETRIES => {
				last_error = Some(AuthError::Login(message));
			}
			RefreshStep::Retry(message) | RefreshStep::Fatal(message) => {
				return Err(AuthError::Login(message));
			}
		}
	}
	Err(last_error
		.unwrap_or_else(|| AuthError::Login("Kimi Code token refresh failed".to_string())))
}

pub struct KimiCodingOAuth;

impl OAuthFlow for KimiCodingOAuth {
	fn name(&self) -> &str {
		"Kimi Code (subscription)"
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
			let oauth_host = get_oauth_host();
			let device = start_device_authorization(&oauth_host, interaction.signal()).await?;
			interaction.notify(AuthEvent::DeviceCode {
				user_code: device.user_code.clone(),
				verification_uri: device.verification_uri_complete.clone(),
				open_browser_on_confirm: true,
				copy_code_on_confirm: true,
				interval_seconds: Some(device.interval_seconds),
				expires_in_seconds: Some(device.expires_in_seconds),
			});
			let token = poll_for_token(&oauth_host, &device, interaction.signal()).await?;
			Ok(OAuthCredential {
				access: token.access,
				refresh: token.refresh,
				expires: token.expires,
				extra: Default::default(),
			})
		})
	}

	fn refresh(
		&self,
		credential: &OAuthCredential,
	) -> BoxFuture<'_, Result<OAuthCredential, AuthError>> {
		let refresh = credential.refresh.clone();
		Box::pin(async move {
			let token = refresh_token(
				&get_oauth_host(),
				&refresh,
				tokio_util::sync::CancellationToken::new(),
			)
			.await?;
			Ok(OAuthCredential {
				access: token.access,
				refresh: token.refresh,
				expires: token.expires,
				extra: Default::default(),
			})
		})
	}

	fn to_auth(&self, credential: &OAuthCredential) -> Result<ModelAuth, AuthError> {
		Ok(ModelAuth {
			api_key: None,
			headers: [(
				"Authorization".to_string(),
				format!("Bearer {}", credential.access),
			)]
			.into_iter()
			.collect(),
			base_url: None,
		})
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use serde_json::json;

	#[test]
	fn oauth_host_prefers_scoped_overrides_and_strips_trailing_slashes() {
		assert_eq!(oauth_host_from(None), "https://auth.kimi.com");
		assert_eq!(
			oauth_host_from(Some("http://localhost:9000")),
			"http://localhost:9000"
		);
		assert_eq!(
			oauth_host_from(Some("http://localhost:9000///")),
			"http://localhost:9000"
		);
		assert_eq!(oauth_host_from(Some("")), "https://auth.kimi.com");
	}

	#[test]
	fn device_authorization_rejects_untrusted_uris_and_bad_fields() {
		let ok = json!({
			"device_code": "dc",
			"user_code": "ABCD",
			"verification_uri": "https://auth.kimi.com/device",
			"verification_uri_complete": "https://auth.kimi.com/device?code=ABCD"
		});
		let device = parse_device_authorization(200, "", Some(&ok)).unwrap();
		assert_eq!(device.interval_seconds, DEFAULT_POLL_INTERVAL_SECONDS);
		assert_eq!(device.expires_in_seconds, DEVICE_CODE_TIMEOUT_SECONDS);
		assert_eq!(
			device.verification_uri_complete,
			"https://auth.kimi.com/device?code=ABCD"
		);

		// Status is checked before the body.
		let error = parse_device_authorization(503, "down", Some(&ok)).unwrap_err();
		assert_eq!(
			error.to_string(),
			"Kimi Code device authorization failed with status 503: down"
		);
		let error = parse_device_authorization(500, "", Some(&ok)).unwrap_err();
		assert_eq!(
			error.to_string(),
			"Kimi Code device authorization failed with status 500"
		);

		// A non-http(s) verification URI is rejected at the boundary.
		let mut untrusted = ok.clone();
		untrusted["verification_uri"] = json!("file:///etc/passwd");
		let error = parse_device_authorization(200, "", Some(&untrusted)).unwrap_err();
		assert_eq!(
			error.to_string(),
			r#"Invalid Kimi Code device authorization response: {"device_code":"dc","user_code":"ABCD","verification_uri":"file:///etc/passwd","verification_uri_complete":"https://auth.kimi.com/device?code=ABCD"}"#
		);

		// Missing fields stringify as null.
		let error =
			parse_device_authorization(200, "", Some(&json!({"device_code": "dc"}))).unwrap_err();
		assert_eq!(
			error.to_string(),
			r#"Invalid Kimi Code device authorization response: {"device_code":"dc"}"#
		);
		let error = parse_device_authorization(200, "", None).unwrap_err();
		assert_eq!(
			error.to_string(),
			"Invalid Kimi Code device authorization response: null"
		);

		// Server-provided positive timings win.
		let device = parse_device_authorization(
			200,
			"",
			Some(&{
				let mut value = ok;
				value["interval"] = json!(3);
				value["expires_in"] = json!(600);
				value
			}),
		)
		.unwrap();
		assert_eq!(device.interval_seconds, 3);
		assert_eq!(device.expires_in_seconds, 600);
	}

	#[test]
	fn token_response_missing_fields_messages() {
		let error = parse_token_response(None, "poll").unwrap_err();
		assert_eq!(
			error.to_string(),
			"Kimi Code token poll response missing fields: null"
		);
		let error = parse_token_response(
			Some(&json!({"access_token": "a", "expires_in": 10})),
			"refresh",
		)
		.unwrap_err();
		assert_eq!(
			error.to_string(),
			r#"Kimi Code token refresh response missing fields: {"access_token":"a","expires_in":10}"#
		);
		// Blank or non-positive fields fail like missing ones.
		let error = parse_token_response(
			Some(&json!({"access_token": "", "refresh_token": "r", "expires_in": 10})),
			"poll",
		)
		.unwrap_err();
		assert!(
			error
				.to_string()
				.starts_with("Kimi Code token poll response missing fields:")
		);
		let error = parse_token_response(
			Some(&json!({"access_token": "a", "refresh_token": "r", "expires_in": 0})),
			"poll",
		)
		.unwrap_err();
		assert!(
			error
				.to_string()
				.starts_with("Kimi Code token poll response missing fields:")
		);

		let token = parse_token_response(
			Some(&json!({"access_token": "a", "refresh_token": "r", "expires_in": 120})),
			"poll",
		)
		.unwrap();
		assert_eq!(token.access, "a");
		assert_eq!(token.refresh, "r");
		assert!(token.expires >= now_ms() + 120 * 1000 - 1000);
	}

	#[test]
	fn poll_result_classifies_device_token_polls() {
		let complete = poll_result(
			200,
			"",
			Some(&json!({"access_token": "a", "refresh_token": "r", "expires_in": 10})),
		);
		assert!(matches!(complete, DevicePollResult::Complete(_)));
		// A 2xx without a usable token response surfaces the parse failure.
		let failed = poll_result(200, "", Some(&json!({"access_token": "a"})));
		assert_eq!(
			match failed {
				DevicePollResult::Failed { message } => message,
				_ => panic!("expected failure"),
			},
			"Kimi Code token poll response missing fields: {\"access_token\":\"a\"}"
		);
		assert!(matches!(
			poll_result(400, "", Some(&json!({"error": "authorization_pending"}))),
			DevicePollResult::Pending
		));
		assert!(matches!(
			poll_result(400, "", Some(&json!({"error": "slow_down", "interval": 8}))),
			DevicePollResult::SlowDown {
				interval_seconds: Some(8)
			}
		));
		assert!(matches!(
			poll_result(400, "", Some(&json!({"error": "slow_down", "interval": 0}))),
			DevicePollResult::SlowDown {
				interval_seconds: None
			}
		));
		let expired = poll_result(400, "", Some(&json!({"error": "expired_token"})));
		assert_eq!(
			match expired {
				DevicePollResult::Failed { message } => message,
				_ => panic!("expected failure"),
			},
			"Kimi Code device authorization expired. Please restart login."
		);
		let denied = poll_result(403, "", Some(&json!({"error": "access_denied"})));
		assert_eq!(
			match denied {
				DevicePollResult::Failed { message } => message,
				_ => panic!("expected failure"),
			},
			"Kimi Code login was denied."
		);
		let other = poll_result(
			400,
			"",
			Some(&json!({"error": "nope", "error_description": "why"})),
		);
		assert_eq!(
			match other {
				DevicePollResult::Failed { message } => message,
				_ => panic!("expected failure"),
			},
			"Kimi Code device token request failed (status 400): nope: why"
		);
		let no_error = poll_result(418, "", Some(&json!({})));
		assert_eq!(
			match no_error {
				DevicePollResult::Failed { message } => message,
				_ => panic!("expected failure"),
			},
			"Kimi Code device token request failed (status 418)"
		);
		// 5xx surfaces the raw body text before any JSON handling.
		let server_error = poll_result(502, "bad gateway", Some(&json!({"error": "ignored"})));
		assert_eq!(
			match server_error {
				DevicePollResult::Failed { message } => message,
				_ => panic!("expected failure"),
			},
			"Kimi Code device token request failed with status 502: bad gateway"
		);
	}

	#[test]
	fn refresh_step_classifies_one_attempt() {
		let done = refresh_step(
			200,
			Some(&json!({"access_token": "a", "refresh_token": "r", "expires_in": 10})),
		);
		assert!(matches!(done, RefreshStep::Done(_)));

		for status in [401, 403] {
			let step = refresh_step(status, Some(&json!({"error_description": "stale"})));
			assert_eq!(
				match step {
					RefreshStep::Unauthorized(message) => message,
					_ => panic!("expected unauthorized"),
				},
				format!("Kimi Code token refresh unauthorized (status {status}): stale")
			);
		}
		let step = refresh_step(400, Some(&json!({"error": "invalid_grant"})));
		assert_eq!(
			match step {
				RefreshStep::Unauthorized(message) => message,
				_ => panic!("expected unauthorized"),
			},
			"Kimi Code token refresh unauthorized (status 400)"
		);

		let step = refresh_step(429, Some(&json!({})));
		assert!(
			matches!(step, RefreshStep::Retry(message) if message == "Kimi Code token refresh failed with status 429")
		);
		let step = refresh_step(503, Some(&json!({})));
		assert!(matches!(step, RefreshStep::Retry(_)));

		let step = refresh_step(400, Some(&json!({"error": "bad"})));
		assert_eq!(
			match step {
				RefreshStep::Fatal(message) => message,
				_ => panic!("expected fatal"),
			},
			r#"Kimi Code token refresh failed with status 400: {"error":"bad"}"#
		);
		let step = refresh_step(400, None);
		assert_eq!(
			match step {
				RefreshStep::Fatal(message) => message,
				_ => panic!("expected fatal"),
			},
			"Kimi Code token refresh failed with status 400: null"
		);
	}
}
