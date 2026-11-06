//! xAI OAuth device-code flow — port of `pi/packages/ai/src/auth/oauth/xai.ts`.

use futures::future::BoxFuture;

use super::{now_ms, post_form_fields};
use crate::auth::device::{DevicePollResult, poll_oauth_device_code_flow};
use crate::auth::types::{
	AuthError, AuthEvent, AuthInteraction, LoginOptions, ModelAuth, OAuthCredential, OAuthFlow,
};

const XAI_CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const XAI_SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";
const XAI_DEVICE_CODE_URL: &str = "https://auth.x.ai/oauth2/device/code";
const XAI_TOKEN_URL: &str = "https://auth.x.ai/oauth2/token";
// Refresh slightly before the reported expiry to avoid using a token that dies mid-request.
const REFRESH_SKEW_MS: i64 = 5 * 60 * 1000;
const DEFAULT_TOKEN_LIFETIME_SECONDS: f64 = 3600.0;

const FORM_HEADERS: &[(&str, &str)] = &[
	("Accept", "application/json"),
	("Content-Type", "application/x-www-form-urlencoded"),
];

/// One xAI OAuth HTTP observation (`OAuthHttpResponse`): status and the parsed
/// JSON body (non-object bodies collapse to `{}`).
#[derive(Debug)]
struct HttpResponse {
	ok: bool,
	status: u16,
	body: serde_json::Value,
}

#[derive(Debug)]
struct XaiDeviceCode {
	device_code: String,
	user_code: String,
	verification_uri: String,
	verification_uri_complete: Option<String>,
	interval_seconds: Option<u64>,
	expires_in_seconds: f64,
}

fn required_string<'a>(body: &'a serde_json::Value, field: &str) -> Result<&'a str, AuthError> {
	body.get(field)
		.and_then(|value| value.as_str())
		.filter(|value| !value.is_empty())
		.ok_or_else(|| AuthError::Login(format!("Invalid xAI OAuth response field: {field}")))
}

fn positive_number(body: &serde_json::Value, field: &str) -> Result<f64, AuthError> {
	let value = body
		.get(field)
		.and_then(|value| value.as_f64())
		.filter(|value| value.is_finite() && *value > 0.0);
	match value {
		Some(value) => Ok(value),
		None => Err(AuthError::Login(format!(
			"Invalid xAI OAuth response field: {field}"
		))),
	}
}

/// The verification URI is opened in the user's browser; force it to be an
/// https URL so a malicious response cannot make `open` launch something else.
fn validate_verification_uri(raw: &str) -> Result<String, AuthError> {
	let url = reqwest::Url::parse(raw).map_err(|_| {
		AuthError::Login("Untrusted verification URI in xAI OAuth response".to_string())
	})?;
	if url.scheme() != "https" {
		return Err(AuthError::Login(
			"Untrusted verification URI in xAI OAuth response".to_string(),
		));
	}
	Ok(url.to_string())
}

async fn post_form(
	url: &str,
	fields: &[(&str, &str)],
	signal: tokio_util::sync::CancellationToken,
) -> Result<HttpResponse, AuthError> {
	let response = post_form_fields(url, fields, FORM_HEADERS, signal).await?;
	let status = response.status();
	let ok = status.is_success();
	let text = response
		.text()
		.await
		.map_err(|error| AuthError::Login(error.to_string()))?;
	let body: serde_json::Value = match serde_json::from_str(&text) {
		Ok(parsed @ serde_json::Value::Object(_)) => parsed,
		Ok(_) => serde_json::json!({}),
		Err(_) => {
			return Err(AuthError::Login(format!(
				"xAI OAuth returned invalid JSON (HTTP {})",
				status.as_u16()
			)));
		}
	};
	Ok(HttpResponse {
		ok,
		status: status.as_u16(),
		body,
	})
}

fn request_failure(action: &str, response: &HttpResponse) -> AuthError {
	let error = response.body.get("error").and_then(|value| value.as_str());
	let description = response
		.body
		.get("error_description")
		.and_then(|value| value.as_str());
	let detail = [error, description]
		.into_iter()
		.flatten()
		.collect::<Vec<_>>()
		.join(": ");
	AuthError::Login(match detail.is_empty() {
		true => format!("xAI OAuth {action} failed (HTTP {})", response.status),
		false => format!(
			"xAI OAuth {action} failed (HTTP {}): {detail}",
			response.status
		),
	})
}

fn parse_device_code(body: &serde_json::Value) -> Result<XaiDeviceCode, AuthError> {
	// RFC 8628 allows interval 0 (no minimum wait); fall back to the poller's
	// default instead of failing on non-positive or malformed values.
	let interval_seconds = body
		.get("interval")
		.and_then(|value| value.as_f64())
		.filter(|value| value.is_finite() && *value > 0.0)
		.map(|value| value as u64);
	let verification_uri_complete = match body.get("verification_uri_complete") {
		Some(serde_json::Value::String(raw)) if !raw.is_empty() => {
			Some(validate_verification_uri(raw)?)
		}
		_ => None,
	};
	Ok(XaiDeviceCode {
		device_code: required_string(body, "device_code")?.to_string(),
		user_code: required_string(body, "user_code")?.to_string(),
		verification_uri: validate_verification_uri(required_string(body, "verification_uri")?)?,
		verification_uri_complete,
		interval_seconds,
		expires_in_seconds: positive_number(body, "expires_in")?,
	})
}

fn credentials_from_token_response(
	body: &serde_json::Value,
	previous_refresh_token: Option<&str>,
) -> Result<OAuthCredential, AuthError> {
	let access = required_string(body, "access_token")?.to_string();
	// xAI may omit refresh_token on refresh when the token is not rotated.
	let refresh = match body.get("refresh_token") {
		None => match previous_refresh_token {
			Some(previous) => previous.to_string(),
			None => required_string(body, "refresh_token")?.to_string(),
		},
		Some(_) => required_string(body, "refresh_token")?.to_string(),
	};
	let expires_in_seconds = match body.get("expires_in") {
		None => DEFAULT_TOKEN_LIFETIME_SECONDS,
		Some(_) => positive_number(body, "expires_in")?,
	};
	Ok(OAuthCredential {
		access,
		refresh,
		expires: now_ms() + (expires_in_seconds * 1000.0) as i64 - REFRESH_SKEW_MS,
		extra: Default::default(),
	})
}

async fn request_device_code(
	signal: tokio_util::sync::CancellationToken,
) -> Result<XaiDeviceCode, AuthError> {
	let response = post_form(
		XAI_DEVICE_CODE_URL,
		&[
			("client_id", XAI_CLIENT_ID),
			("scope", XAI_SCOPE),
			("referrer", "pi"),
		],
		signal,
	)
	.await?;
	if !response.ok {
		return Err(request_failure("device authorization", &response));
	}
	parse_device_code(&response.body)
}

/// Classify one device-token poll response (pure; the poller drives it).
fn poll_result(response: &HttpResponse) -> Result<DevicePollResult<OAuthCredential>, AuthError> {
	if response.ok {
		return Ok(DevicePollResult::Complete(credentials_from_token_response(
			&response.body,
			None,
		)?));
	}
	let error = response.body.get("error").and_then(|value| value.as_str());
	match error {
		Some("authorization_pending") => Ok(DevicePollResult::Pending),
		Some("slow_down") => {
			let interval_seconds = response
				.body
				.get("interval")
				.and_then(|value| value.as_f64())
				.filter(|value| value.is_finite() && *value >= 0.0)
				.map(|value| value as u64);
			Ok(DevicePollResult::SlowDown { interval_seconds })
		}
		Some("access_denied" | "authorization_denied") => Ok(DevicePollResult::Failed {
			message: "xAI device authorization was denied".to_string(),
		}),
		Some("expired_token") => Ok(DevicePollResult::Failed {
			message: "xAI device code expired".to_string(),
		}),
		_ => Ok(DevicePollResult::Failed {
			message: request_failure("device token polling", response).to_string(),
		}),
	}
}

async fn poll_for_tokens(
	device: XaiDeviceCode,
	signal: tokio_util::sync::CancellationToken,
) -> Result<OAuthCredential, AuthError> {
	poll_oauth_device_code_flow(
		device.interval_seconds,
		Some(device.expires_in_seconds as u64),
		true,
		signal.clone(),
		move || {
			let device_code = device.device_code.clone();
			let signal = signal.clone();
			async move {
				let response = post_form(
					XAI_TOKEN_URL,
					&[
						("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
						("client_id", XAI_CLIENT_ID),
						("device_code", &device_code),
					],
					signal,
				)
				.await?;
				poll_result(&response)
			}
		},
	)
	.await
}

async fn refresh_xai_token(
	refresh_token: &str,
	signal: tokio_util::sync::CancellationToken,
) -> Result<OAuthCredential, AuthError> {
	let response = post_form(
		XAI_TOKEN_URL,
		&[
			("grant_type", "refresh_token"),
			("client_id", XAI_CLIENT_ID),
			("refresh_token", refresh_token),
		],
		signal,
	)
	.await?;
	if !response.ok {
		return Err(request_failure("token refresh", &response));
	}
	credentials_from_token_response(&response.body, Some(refresh_token))
}

pub struct XaiOAuth;

impl OAuthFlow for XaiOAuth {
	fn name(&self) -> &str {
		"xAI (Grok/X subscription)"
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
			let device = request_device_code(interaction.signal()).await?;
			interaction.notify(AuthEvent::DeviceCode {
				user_code: device.user_code.clone(),
				verification_uri: device
					.verification_uri_complete
					.clone()
					.unwrap_or_else(|| device.verification_uri.clone()),
				open_browser_on_confirm: true,
				copy_code_on_confirm: true,
				interval_seconds: device.interval_seconds,
				expires_in_seconds: Some(device.expires_in_seconds as u64),
			});
			poll_for_tokens(device, interaction.signal()).await
		})
	}

	fn refresh(
		&self,
		credential: &OAuthCredential,
	) -> BoxFuture<'_, Result<OAuthCredential, AuthError>> {
		let token = credential.refresh.clone();
		Box::pin(async move {
			refresh_xai_token(&token, tokio_util::sync::CancellationToken::new()).await
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

	fn response(status: u16, body: serde_json::Value) -> HttpResponse {
		HttpResponse {
			ok: (200..300).contains(&status),
			status,
			body,
		}
	}

	#[test]
	fn parse_device_code_accepts_https_uris_and_normalizes() {
		let device = parse_device_code(&json!({
			"device_code": "dc",
			"user_code": "ABCD-1234",
			"verification_uri": "https://auth.x.ai/device",
			"verification_uri_complete": "https://auth.x.ai/device?code=ABCD-1234",
			"interval": 5,
			"expires_in": 600
		}))
		.unwrap();
		assert_eq!(device.device_code, "dc");
		assert_eq!(device.verification_uri, "https://auth.x.ai/device");
		assert_eq!(
			device.verification_uri_complete.as_deref(),
			Some("https://auth.x.ai/device?code=ABCD-1234")
		);
		assert_eq!(device.interval_seconds, Some(5));
		assert_eq!(device.expires_in_seconds, 600.0);
	}

	#[test]
	fn parse_device_code_rejects_non_https_verification_uris() {
		let base = json!({
			"device_code": "dc",
			"user_code": "ABCD-1234",
			"expires_in": 600
		});
		for uri in ["http://auth.x.ai/device", "$(id>/tmp/pwned)"] {
			let mut body = base.clone();
			body["verification_uri"] = json!(uri);
			let error = parse_device_code(&body).unwrap_err();
			assert_eq!(
				error.to_string(),
				"Untrusted verification URI in xAI OAuth response"
			);
		}
		// An empty verification_uri fails the required-field gate first, exactly
		// like upstream's validateVerificationUri(requiredString(...)) ordering.
		let mut body = base;
		body["verification_uri"] = json!("");
		let error = parse_device_code(&body).unwrap_err();
		assert_eq!(
			error.to_string(),
			"Invalid xAI OAuth response field: verification_uri"
		);
	}

	#[test]
	fn parse_device_code_field_errors_name_the_field() {
		let error = parse_device_code(&json!({"user_code": "x", "expires_in": 10})).unwrap_err();
		assert_eq!(
			error.to_string(),
			"Invalid xAI OAuth response field: device_code"
		);
		// Upstream evaluates device_code, user_code, verification_uri, then
		// expires_in, so a missing verification_uri wins over expires_in.
		let error = parse_device_code(&json!({"device_code": "dc", "user_code": "x"})).unwrap_err();
		assert_eq!(
			error.to_string(),
			"Invalid xAI OAuth response field: verification_uri"
		);
		// RFC 8628 allows interval 0; it falls back to the poller default.
		let device = parse_device_code(&json!({
			"device_code": "dc",
			"user_code": "x",
			"verification_uri": "https://auth.x.ai/device",
			"interval": 0,
			"expires_in": 10
		}))
		.unwrap();
		assert_eq!(device.interval_seconds, None);
	}

	#[test]
	fn credentials_fall_back_to_the_previous_refresh_token() {
		let base = json!({"access_token": "at"});
		let credential = credentials_from_token_response(&base, Some("prev")).unwrap();
		assert_eq!(credential.refresh, "prev");
		// Without a previous token the missing field is an error.
		let error = credentials_from_token_response(&base, None).unwrap_err();
		assert_eq!(
			error.to_string(),
			"Invalid xAI OAuth response field: refresh_token"
		);
		// An explicit null refresh_token is not the same as omitted.
		let mut body = base.clone();
		body["refresh_token"] = serde_json::Value::Null;
		let error = credentials_from_token_response(&body, Some("prev")).unwrap_err();
		assert_eq!(
			error.to_string(),
			"Invalid xAI OAuth response field: refresh_token"
		);
	}

	#[test]
	fn credentials_apply_lifetime_default_and_refresh_skew() {
		let now = now_ms();
		let credential = credentials_from_token_response(
			&json!({
				"access_token": "at",
				"refresh_token": "rt",
				"expires_in": 7200
			}),
			None,
		)
		.unwrap();
		assert_eq!(credential.expires, now + 7200 * 1000 - REFRESH_SKEW_MS);
		// Omitted expires_in falls back to the default lifetime.
		let credential = credentials_from_token_response(
			&json!({"access_token": "at", "refresh_token": "rt"}),
			None,
		)
		.unwrap();
		assert_eq!(credential.expires, now + 3600 * 1000 - REFRESH_SKEW_MS);
	}

	#[test]
	fn request_failure_formats_error_and_description() {
		let failure = request_failure(
			"device authorization",
			&response(
				400,
				json!({"error": "bad_request", "error_description": "nope"}),
			),
		);
		assert_eq!(
			failure.to_string(),
			"xAI OAuth device authorization failed (HTTP 400): bad_request: nope"
		);
		let failure = request_failure(
			"token refresh",
			&response(500, json!({"error": "server_error"})),
		);
		assert_eq!(
			failure.to_string(),
			"xAI OAuth token refresh failed (HTTP 500): server_error"
		);
		let failure = request_failure("token refresh", &response(503, json!({})));
		assert_eq!(
			failure.to_string(),
			"xAI OAuth token refresh failed (HTTP 503)"
		);
	}

	#[test]
	fn poll_result_classifies_device_token_responses() {
		assert!(matches!(
			poll_result(&response(
				200,
				json!({"access_token": "at", "refresh_token": "rt", "expires_in": 10})
			))
			.unwrap(),
			DevicePollResult::Complete(_)
		));
		assert!(matches!(
			poll_result(&response(400, json!({"error": "authorization_pending"}))).unwrap(),
			DevicePollResult::Pending
		));
		let slow_down =
			poll_result(&response(400, json!({"error": "slow_down", "interval": 7}))).unwrap();
		assert!(matches!(
			slow_down,
			DevicePollResult::SlowDown {
				interval_seconds: Some(7)
			}
		));
		assert!(matches!(
			poll_result(&response(400, json!({"error": "access_denied"}))).unwrap(),
			DevicePollResult::Failed { .. }
		));
		assert!(matches!(
			poll_result(&response(400, json!({"error": "authorization_denied"}))).unwrap(),
			DevicePollResult::Failed { .. }
		));
		let failed = poll_result(&response(400, json!({"error": "expired_token"}))).unwrap();
		assert!(matches!(failed, DevicePollResult::Failed { .. }));
		let other = poll_result(&response(
			400,
			json!({"error": "bad", "error_description": "d"}),
		))
		.unwrap();
		assert_eq!(
			match other {
				DevicePollResult::Failed { message } => message,
				_ => panic!("expected failure"),
			},
			"xAI OAuth device token polling failed (HTTP 400): bad: d"
		);
	}

	#[test]
	fn validate_verification_uri_requires_https() {
		assert_eq!(
			validate_verification_uri("https://auth.x.ai/device").unwrap(),
			"https://auth.x.ai/device"
		);
		assert!(validate_verification_uri("ftp://auth.x.ai/device").is_err());
	}
}
