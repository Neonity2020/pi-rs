//! GitHub Copilot OAuth flow — port of
//! `pi/packages/ai/src/auth/oauth/github-copilot.ts` (device flow).

use std::collections::BTreeMap;

use futures::future::BoxFuture;

use super::now_ms;
use crate::auth::device::{DevicePollResult, poll_oauth_device_code_flow};
use crate::auth::types::{
	AuthError, AuthEvent, AuthInteraction, AuthPromptKind, LoginOptions, ModelAuth,
	OAuthCredential, OAuthFlow,
};

// Base64 of `Iv1.b507a08c87ecfe98` (upstream obfuscates the literal the same way).
const CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";

const COPILOT_HEADERS: &[(&str, &str)] = &[
	("User-Agent", "GitHubCopilotChat/0.35.0"),
	("Editor-Version", "vscode/1.107.0"),
	("Editor-Plugin-Version", "copilot-chat/0.35.0"),
	("Copilot-Integration-Id", "vscode-chat"),
];
const COPILOT_API_VERSION: &str = "2026-06-01";

const FORM_HEADERS: &[(&str, &str)] = &[
	("Accept", "application/json"),
	("Content-Type", "application/x-www-form-urlencoded"),
	("User-Agent", "GitHubCopilotChat/0.35.0"),
];

#[derive(Debug)]
struct DeviceCodeResponse {
	device_code: String,
	user_code: String,
	verification_uri: String,
	interval: Option<f64>,
	expires_in: f64,
}

#[derive(Debug)]
struct CopilotUrls {
	device_code_url: String,
	access_token_url: String,
	copilot_token_url: String,
}

fn normalize_domain(input: &str) -> Option<String> {
	let trimmed = input.trim();
	if trimmed.is_empty() {
		return None;
	}
	let candidate = if trimmed.contains("://") {
		trimmed.to_string()
	} else {
		format!("https://{trimmed}")
	};
	let url = reqwest::Url::parse(&candidate).ok()?;
	url.host_str().map(str::to_string)
}

fn get_urls(domain: &str) -> CopilotUrls {
	CopilotUrls {
		device_code_url: format!("https://{domain}/login/device/code"),
		access_token_url: format!("https://{domain}/login/oauth/access_token"),
		copilot_token_url: format!("https://api.{domain}/copilot_internal/v2/token"),
	}
}

/// Parse the proxy-ep from a Copilot token and convert to API base URL.
/// Token format: tid=...;exp=...;proxy-ep=proxy.individual.githubcopilot.com;...
/// Returns API URL like https://api.individual.githubcopilot.com
fn base_url_from_token(token: &str) -> Option<String> {
	let start = token.find("proxy-ep=")? + "proxy-ep=".len();
	let rest = &token[start..];
	let end = rest.find(';').unwrap_or(rest.len());
	let proxy_host = &rest[..end];
	if proxy_host.is_empty() {
		return None;
	}
	// Convert proxy.xxx to api.xxx
	let api_host = proxy_host
		.strip_prefix("proxy.")
		.map(|rest| format!("api.{rest}"))
		.unwrap_or_else(|| proxy_host.to_string());
	Some(format!("https://{api_host}"))
}

fn get_base_url(token: Option<&str>, enterprise_domain: Option<&str>) -> String {
	// If we have a token, extract the base URL from proxy-ep
	if let Some(token) = token
		&& let Some(url_from_token) = base_url_from_token(token)
	{
		return url_from_token;
	}
	// Fallback for enterprise or if token parsing fails
	if let Some(enterprise_domain) = enterprise_domain {
		return format!("https://copilot-api.{enterprise_domain}");
	}
	"https://api.individual.githubcopilot.com".to_string()
}

/// One HTTP request through the shared cancellation mapping. Copilot requests
/// carry custom headers; the per-request timeout is set by the rate-limit
/// retry helper (upstream `AbortSignal.timeout(5000)` per attempt).
async fn send(
	method: reqwest::Method,
	url: &str,
	headers: &[(String, String)],
	body: Option<String>,
	signal: tokio_util::sync::CancellationToken,
) -> Result<reqwest::Response, AuthError> {
	let mut request = reqwest::Client::new().request(method, url);
	for (name, value) in headers {
		request = request.header(name, value);
	}
	if let Some(body) = body {
		request = request.body(body);
	}
	crate::auth::http::send_with_cancellation(request, signal).await
}

async fn fetch_json(
	method: reqwest::Method,
	url: &str,
	headers: &[(String, String)],
	body: Option<String>,
	signal: tokio_util::sync::CancellationToken,
) -> Result<serde_json::Value, AuthError> {
	let response = send(method, url, headers, body, signal).await?;
	let status = response.status();
	let text = response
		.text()
		.await
		.map_err(|error| AuthError::Login(error.to_string()))?;
	if !status.is_success() {
		return Err(AuthError::Login(format!(
			"{} {}: {text}",
			status.as_u16(),
			status.canonical_reason().unwrap_or("")
		)));
	}
	serde_json::from_str(&text).map_err(|error| AuthError::Login(error.to_string()))
}

/// IMF-fixdate parser (`Date.parse` on RFC 7231 `Retry-After` dates).
fn parse_http_date(input: &str) -> Option<i64> {
	let trimmed = input.trim();
	let rest = trimmed.strip_suffix(" GMT")?;
	let rest = rest.split_once(", ").map(|(_, rest)| rest).unwrap_or(rest);
	let mut parts = rest.split_whitespace();
	let day: i64 = parts.next()?.parse().ok()?;
	let month = match parts.next()? {
		"Jan" => 1,
		"Feb" => 2,
		"Mar" => 3,
		"Apr" => 4,
		"May" => 5,
		"Jun" => 6,
		"Jul" => 7,
		"Aug" => 8,
		"Sep" => 9,
		"Oct" => 10,
		"Nov" => 11,
		"Dec" => 12,
		_ => return None,
	};
	let year: i64 = parts.next()?.parse().ok()?;
	let mut clock = parts.next()?.split(':');
	let hour: i64 = clock.next()?.parse().ok()?;
	let minute: i64 = clock.next()?.parse().ok()?;
	let second: i64 = clock.next()?.parse().ok()?;
	if clock.next().is_some() || parts.next().is_some() {
		return None;
	}
	// Days-from-civil (Howard Hinnant), valid for the whole representable range.
	let y = if month <= 2 { year - 1 } else { year };
	let era = if y >= 0 { y } else { y - 399 } / 400;
	let yoe = y - era * 400;
	let mp = (month as i64 + 9) % 12;
	let doy = (153 * mp + 2) / 5 + day - 1;
	let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
	let days = era * 146097 + doe - 719468;
	Some(days * 86_400_000 + hour * 3_600_000 + minute * 60_000 + second * 1000)
}

/// Delay before the next 429 retry (`None` = give up and return the response).
fn retry_delay_ms(retry_after: Option<&str>, retry: u32, now: i64) -> Option<i64> {
	let mut delay_ms = 500i64.checked_mul(1i64 << retry.min(30))?;
	if let Some(retry_after) = retry_after {
		let seconds = retry_after
			.trim()
			.parse::<f64>()
			.ok()
			.filter(|value| value.is_finite());
		delay_ms = match seconds {
			Some(seconds) => (seconds * 1000.0) as i64,
			None => parse_http_date(retry_after)? - now,
		};
	}
	Some(delay_ms)
}

async fn fetch_with_rate_limit_retry(
	method: reqwest::Method,
	url: &str,
	headers: &[(String, String)],
	body: Option<String>,
	signal: tokio_util::sync::CancellationToken,
	max_retries: u32,
	max_elapsed_ms: u64,
) -> Result<reqwest::Response, AuthError> {
	let retry_deadline = (max_retries > 0 && max_elapsed_ms > 0)
		.then(|| std::time::Instant::now() + std::time::Duration::from_millis(max_elapsed_ms));
	let mut retry = 0u32;
	loop {
		let request_timeout = std::time::Duration::from_secs(5);
		let mut request = reqwest::Client::new()
			.request(method.clone(), url)
			.timeout(request_timeout);
		for (name, value) in headers {
			request = request.header(name, value);
		}
		if let Some(body) = &body {
			request = request.body(body.clone());
		}
		let response = crate::auth::http::send_with_cancellation(request, signal.clone()).await?;
		if response.status().as_u16() != 429 || retry == max_retries {
			return Ok(response);
		}

		let now = now_ms();
		let retry_after = response
			.headers()
			.get("retry-after")
			.and_then(|value| value.to_str().ok());
		let Some(delay_ms) = retry_delay_ms(retry_after, retry, now) else {
			return Ok(response);
		};
		let delay_ms = delay_ms.max(0);
		if let Some(deadline) = retry_deadline
			&& delay_ms >= (deadline - std::time::Instant::now()).as_millis() as i64
		{
			return Ok(response);
		}
		tokio::select! {
			() = tokio::time::sleep(std::time::Duration::from_millis(delay_ms as u64)) => {}
			() = signal.cancelled() => return Err(AuthError::Login("Login cancelled".to_string())),
		}
		retry += 1;
	}
}

/// The bundled `GITHUB_COPILOT_MODELS` catalog (generated at TS build time from
/// models.dev data) is not ported, so no model id is treated as a known
/// Copilot model and policy enables stay a no-op (`policyModelIds` empty).
fn is_bundled_copilot_model(_model_id: &str) -> bool {
	false
}

#[derive(Debug)]
struct CopilotModelCatalog {
	available_model_ids: Vec<String>,
	policy_model_ids: Vec<String>,
}

/// Pure half of `parseGitHubCopilotModelCatalog`.
fn parse_catalog(
	raw: Option<&serde_json::Value>,
	allow_policy_fallback: bool,
) -> Result<CopilotModelCatalog, AuthError> {
	let data = raw
		.and_then(|raw| raw.get("data"))
		.and_then(|data| data.as_array())
		.ok_or_else(|| AuthError::Login("Invalid Copilot models response".to_string()))?;

	struct AccountModel {
		id: String,
		picker_enabled: bool,
		policy_state: Option<String>,
	}
	let mut account_models = Vec::new();
	for raw_item in data {
		let Some(id) = raw_item.get("id").and_then(|id| id.as_str()) else {
			continue;
		};
		// `supports?.tool_calls === false` excludes the model.
		if raw_item
			.get("capabilities")
			.and_then(|capabilities| capabilities.get("supports"))
			.and_then(|supports| supports.get("tool_calls"))
			== Some(&serde_json::Value::Bool(false))
		{
			continue;
		}
		account_models.push(AccountModel {
			id: id.to_string(),
			picker_enabled: raw_item.get("model_picker_enabled")
				== Some(&serde_json::Value::Bool(true)),
			policy_state: raw_item
				.get("policy")
				.and_then(|policy| policy.get("state"))
				.and_then(|state| state.as_str())
				.map(str::to_string),
		});
	}
	let picker_model_ids = account_models
		.iter()
		.filter(|model| model.picker_enabled && model.policy_state.as_deref() != Some("disabled"))
		.map(|model| model.id.clone())
		.collect::<Vec<_>>();
	let use_policy_fallback = allow_policy_fallback && picker_model_ids.is_empty();
	let available_model_ids = if !picker_model_ids.is_empty() || !allow_policy_fallback {
		picker_model_ids
	} else {
		account_models
			.iter()
			.filter(|model| model.policy_state.as_deref() == Some("enabled"))
			.map(|model| model.id.clone())
			.collect::<Vec<_>>()
	};
	let policy_model_ids = account_models
		.iter()
		.filter(|model| {
			model.policy_state.as_deref() == Some("unconfigured")
				&& is_bundled_copilot_model(&model.id)
				&& (model.picker_enabled || use_policy_fallback)
		})
		.map(|model| model.id.clone())
		.collect::<Vec<_>>();
	Ok(CopilotModelCatalog {
		available_model_ids,
		policy_model_ids,
	})
}

/// Pure half of `startDeviceFlow` (validation + untrusted-URI gate).
fn parse_device_flow(json: Option<&serde_json::Value>) -> Result<DeviceCodeResponse, AuthError> {
	let Some(json) = json else {
		return Err(AuthError::Login("Invalid device code response".to_string()));
	};
	let device_code = json.get("device_code").and_then(|value| value.as_str());
	let user_code = json.get("user_code").and_then(|value| value.as_str());
	let verification_uri = json
		.get("verification_uri")
		.and_then(|value| value.as_str());
	let interval = json.get("interval").and_then(|value| value.as_f64());
	let expires_in = json.get("expires_in").and_then(|value| value.as_f64());
	let fields_valid = [device_code, user_code, verification_uri]
		.into_iter()
		.all(|value| value.is_some_and(|value| !value.is_empty()))
		&& expires_in.is_some();
	if !fields_valid {
		return Err(AuthError::Login(
			"Invalid device code response fields".to_string(),
		));
	}
	// The verification URI is opened in the user's browser and to prevent `open` from
	// opening an executable or similar, we force it to be a URL.
	let parsed_uri = reqwest::Url::parse(verification_uri.unwrap_or_default()).map_err(|_| {
		AuthError::Login("Untrusted verification_uri in device code response".to_string())
	})?;
	if parsed_uri.scheme() != "https" && parsed_uri.scheme() != "http" {
		return Err(AuthError::Login(
			"Untrusted verification_uri in device code response".to_string(),
		));
	}
	Ok(DeviceCodeResponse {
		device_code: device_code.unwrap_or_default().to_string(),
		user_code: user_code.unwrap_or_default().to_string(),
		verification_uri: parsed_uri.to_string(),
		interval,
		expires_in: expires_in.unwrap_or_default(),
	})
}

async fn start_device_flow(
	domain: &str,
	signal: tokio_util::sync::CancellationToken,
) -> Result<DeviceCodeResponse, AuthError> {
	let urls = get_urls(domain);
	let form = [
		("client_id".to_string(), CLIENT_ID.to_string()),
		("scope".to_string(), "read:user".to_string()),
	];
	let body = crate::auth::http::encode_form(&form);
	let json = fetch_json(
		reqwest::Method::POST,
		&urls.device_code_url,
		&FORM_HEADERS
			.iter()
			.map(|(k, v)| (k.to_string(), v.to_string()))
			.collect::<Vec<_>>(),
		Some(body),
		signal,
	)
	.await?;
	parse_device_flow(Some(&json))
}

/// Classify one access-token poll response (pure; the poller drives it).
fn device_token_poll_result(json: Option<&serde_json::Value>) -> DevicePollResult<String> {
	if let Some(access_token) = json
		.and_then(|json| json.get("access_token"))
		.and_then(|value| value.as_str())
	{
		return DevicePollResult::Complete(access_token.to_string());
	}
	let error = json
		.and_then(|json| json.get("error"))
		.and_then(|value| value.as_str());
	if let Some(error) = error {
		let description = json
			.and_then(|json| json.get("error_description"))
			.and_then(|value| value.as_str());
		return match error {
			"authorization_pending" => DevicePollResult::Pending,
			"slow_down" => DevicePollResult::SlowDown {
				interval_seconds: json
					.and_then(|json| json.get("interval"))
					.and_then(|value| value.as_f64())
					.filter(|value| value.is_finite())
					.map(|value| value.max(0.0) as u64),
			},
			_ => DevicePollResult::Failed {
				message: match description {
					Some(description) => format!("Device flow failed: {error}: {description}"),
					None => format!("Device flow failed: {error}"),
				},
			},
		};
	}
	DevicePollResult::Failed {
		message: "Invalid device token response".to_string(),
	}
}

async fn poll_for_github_access_token(
	domain: &str,
	device: &DeviceCodeResponse,
	signal: tokio_util::sync::CancellationToken,
) -> Result<String, AuthError> {
	let urls = get_urls(domain);
	let device_code = device.device_code.clone();
	let signal = signal.clone();
	poll_oauth_device_code_flow(
		device.interval.map(|value| value.max(0.0) as u64),
		Some(device.expires_in.max(0.0) as u64),
		true,
		signal.clone(),
		move || {
			let device_code = device_code.clone();
			let access_token_url = urls.access_token_url.clone();
			let signal = signal.clone();
			async move {
				let form = [
					("client_id".to_string(), CLIENT_ID.to_string()),
					("device_code".to_string(), device_code),
					(
						"grant_type".to_string(),
						"urn:ietf:params:oauth:grant-type:device_code".to_string(),
					),
				];
				let json = fetch_json(
					reqwest::Method::POST,
					&access_token_url,
					&FORM_HEADERS
						.iter()
						.map(|(k, v)| (k.to_string(), v.to_string()))
						.collect::<Vec<_>>(),
					Some(crate::auth::http::encode_form(&form)),
					signal,
				)
				.await?;
				Ok(device_token_poll_result(Some(&json)))
			}
		},
	)
	.await
}

/// Pure half of `refreshGitHubCopilotAccessToken`.
fn parse_copilot_token_response(
	json: Option<&serde_json::Value>,
	enterprise_domain: Option<&str>,
) -> Result<OAuthCredential, AuthError> {
	let Some(json) = json else {
		return Err(AuthError::Login(
			"Invalid Copilot token response".to_string(),
		));
	};
	let token = json.get("token").and_then(|value| value.as_str());
	let expires_at = json.get("expires_at").and_then(|value| value.as_f64());
	match (token, expires_at) {
		(Some(token), Some(expires_at)) => {
			let mut extra = BTreeMap::new();
			if let Some(enterprise_domain) = enterprise_domain {
				extra.insert(
					"enterpriseUrl".to_string(),
					serde_json::Value::String(enterprise_domain.to_string()),
				);
			}
			Ok(OAuthCredential {
				refresh: String::new(),
				access: token.to_string(),
				expires: (expires_at * 1000.0) as i64 - 5 * 60 * 1000,
				extra,
			})
		}
		_ => Err(AuthError::Login(
			"Invalid Copilot token response fields".to_string(),
		)),
	}
}

async fn refresh_copilot_access_token(
	refresh_token: &str,
	enterprise_domain: Option<&str>,
	signal: tokio_util::sync::CancellationToken,
) -> Result<OAuthCredential, AuthError> {
	let domain = enterprise_domain.unwrap_or("github.com");
	let urls = get_urls(domain);
	let mut headers = vec![
		("Accept".to_string(), "application/json".to_string()),
		(
			"Authorization".to_string(),
			format!("Bearer {refresh_token}"),
		),
	];
	for (name, value) in COPILOT_HEADERS {
		headers.push((name.to_string(), value.to_string()));
	}
	let json = fetch_json(
		reqwest::Method::GET,
		&urls.copilot_token_url,
		&headers,
		None,
		signal,
	)
	.await?;
	let mut credential = parse_copilot_token_response(Some(&json), enterprise_domain)?;
	credential.refresh = refresh_token.to_string();
	Ok(credential)
}

async fn fetch_models(
	copilot_token: &str,
	enterprise_domain: Option<&str>,
	signal: tokio_util::sync::CancellationToken,
	max_retries: u32,
	max_elapsed_ms: u64,
) -> Result<CopilotModelCatalog, AuthError> {
	let base_url = get_base_url(Some(copilot_token), enterprise_domain);
	// Some Individual accounts return false for every picker flag despite explicit enabled policies.
	// Limit the fallback to that endpoint so other account types keep strict picker semantics.
	let allow_policy_fallback = base_url == "https://api.individual.githubcopilot.com";
	let mut headers = vec![
		("Accept".to_string(), "application/json".to_string()),
		(
			"Authorization".to_string(),
			format!("Bearer {copilot_token}"),
		),
	];
	for (name, value) in COPILOT_HEADERS {
		headers.push((name.to_string(), value.to_string()));
	}
	headers.push((
		"X-GitHub-Api-Version".to_string(),
		COPILOT_API_VERSION.to_string(),
	));
	let response = fetch_with_rate_limit_retry(
		reqwest::Method::GET,
		&format!("{base_url}/models"),
		&headers,
		None,
		signal,
		max_retries,
		max_elapsed_ms,
	)
	.await?;
	let status = response.status();
	let text = response
		.text()
		.await
		.map_err(|error| AuthError::Login(error.to_string()))?;
	if !status.is_success() {
		return Err(AuthError::Login(format!(
			"{} {}: {text}",
			status.as_u16(),
			status.canonical_reason().unwrap_or("")
		)));
	}
	let json: serde_json::Value =
		serde_json::from_str(&text).map_err(|error| AuthError::Login(error.to_string()))?;
	parse_catalog(Some(&json), allow_policy_fallback)
}

/// Enable a model for the user's GitHub Copilot account.
/// This is required for some models (like Claude, Grok) before they can be used.
async fn enable_model(
	token: &str,
	model_id: &str,
	enterprise_domain: Option<&str>,
	signal: tokio_util::sync::CancellationToken,
) -> Result<bool, AuthError> {
	let base_url = get_base_url(Some(token), enterprise_domain);
	let url = format!("{base_url}/models/{model_id}/policy");
	let mut headers = vec![
		("Content-Type".to_string(), "application/json".to_string()),
		("Authorization".to_string(), format!("Bearer {token}")),
	];
	for (name, value) in COPILOT_HEADERS {
		headers.push((name.to_string(), value.to_string()));
	}
	headers.push(("openai-intent".to_string(), "chat-policy".to_string()));
	headers.push(("x-interaction-type".to_string(), "chat-policy".to_string()));
	let response = match fetch_with_rate_limit_retry(
		reqwest::Method::POST,
		&url,
		&headers,
		Some(serde_json::json!({"state": "enabled"}).to_string()),
		signal.clone(),
		2,
		5000,
	)
	.await
	{
		Ok(response) => response,
		Err(error) => {
			if signal.is_cancelled() {
				return Err(error);
			}
			return Ok(false);
		}
	};
	if response.status().as_u16() == 429 {
		let status = response.status();
		let text = response
			.text()
			.await
			.map_err(|error| AuthError::Login(error.to_string()))?;
		return Err(AuthError::Login(format!(
			"{} {}: {text}",
			status.as_u16(),
			status.canonical_reason().unwrap_or("")
		)));
	}
	Ok(response.status().is_success())
}

/// Enable the requested GitHub Copilot models and return the successful IDs.
/// Policy updates are best effort; exhausted rate limiting stops the batch.
async fn enable_models(
	token: &str,
	model_ids: &[String],
	enterprise_domain: Option<&str>,
	signal: tokio_util::sync::CancellationToken,
) -> Result<Vec<String>, AuthError> {
	let mut enabled_model_ids = Vec::new();
	for model_id in model_ids {
		match enable_model(token, model_id, enterprise_domain, signal.clone()).await {
			Ok(true) => enabled_model_ids.push(model_id.clone()),
			Ok(false) => {}
			Err(error) => {
				if signal.is_cancelled() {
					return Err(error);
				}
				break;
			}
		}
	}
	Ok(enabled_model_ids)
}

fn copilot_enterprise_domain(credential: &OAuthCredential) -> Option<String> {
	let enterprise_url = credential.extra.get("enterpriseUrl")?.as_str()?;
	if enterprise_url.is_empty() {
		return None;
	}
	normalize_domain(enterprise_url)
}

fn available_model_ids_extra(credential: &mut OAuthCredential, available_model_ids: Vec<String>) {
	credential.extra.insert(
		"availableModelIds".to_string(),
		serde_json::Value::Array(
			available_model_ids
				.into_iter()
				.map(serde_json::Value::String)
				.collect(),
		),
	);
}

async fn login_github_copilot(
	interaction: &dyn AuthInteraction,
) -> Result<OAuthCredential, AuthError> {
	let input = interaction
		.prompt(crate::auth::types::AuthPrompt {
			kind: AuthPromptKind::Text {
				message: "GitHub Enterprise URL/domain (blank for github.com)".to_string(),
				placeholder: Some("company.ghe.com".to_string()),
			},
			signal: interaction.signal(),
		})
		.await?;
	if interaction.signal().is_cancelled() {
		return Err(AuthError::Login("Login cancelled".to_string()));
	}

	let trimmed = input.trim();
	let enterprise_domain = normalize_domain(&input);
	if !trimmed.is_empty() && enterprise_domain.is_none() {
		return Err(AuthError::Login(
			"Invalid GitHub Enterprise URL/domain".to_string(),
		));
	}
	let enterprise_domain = enterprise_domain.unwrap_or_else(|| "github.com".to_string());

	let device = start_device_flow(&enterprise_domain, interaction.signal()).await?;
	interaction.notify(AuthEvent::DeviceCode {
		user_code: device.user_code.clone(),
		verification_uri: device.verification_uri.clone(),
		open_browser_on_confirm: true,
		copy_code_on_confirm: true,
		interval_seconds: device.interval.map(|value| value.max(0.0) as u64),
		expires_in_seconds: Some(device.expires_in.max(0.0) as u64),
	});

	let github_access_token =
		poll_for_github_access_token(&enterprise_domain, &device, interaction.signal()).await?;
	let mut credentials = refresh_copilot_access_token(
		&github_access_token,
		Some(&enterprise_domain),
		interaction.signal(),
	)
	.await?;
	let models = fetch_models(
		&credentials.access,
		Some(&enterprise_domain),
		interaction.signal(),
		2,
		5000,
	)
	.await?;
	let mut enabled_model_ids = Vec::new();
	if !models.policy_model_ids.is_empty() {
		interaction.notify(AuthEvent::Progress {
			message: "Enabling models...".to_string(),
		});
		enabled_model_ids = enable_models(
			&credentials.access,
			&models.policy_model_ids,
			Some(&enterprise_domain),
			interaction.signal(),
		)
		.await?;
	}
	let mut merged = Vec::new();
	for model_id in models
		.available_model_ids
		.into_iter()
		.chain(enabled_model_ids)
	{
		if !merged.contains(&model_id) {
			merged.push(model_id);
		}
	}
	available_model_ids_extra(&mut credentials, merged);
	Ok(credentials)
}

pub struct GitHubCopilotOAuth;

impl OAuthFlow for GitHubCopilotOAuth {
	fn name(&self) -> &str {
		"GitHub Copilot"
	}

	fn is_subscription(&self) -> bool {
		true
	}

	fn login<'a>(
		&'a self,
		interaction: &'a dyn AuthInteraction,
		_options: &'a LoginOptions,
	) -> BoxFuture<'a, Result<OAuthCredential, AuthError>> {
		Box::pin(async move { login_github_copilot(interaction).await })
	}

	fn refresh(
		&self,
		credential: &OAuthCredential,
	) -> BoxFuture<'_, Result<OAuthCredential, AuthError>> {
		let refresh = credential.refresh.clone();
		let enterprise_domain = copilot_enterprise_domain(credential);
		Box::pin(async move {
			let signal = tokio_util::sync::CancellationToken::new();
			let mut credentials = refresh_copilot_access_token(
				&refresh,
				enterprise_domain.as_deref(),
				signal.clone(),
			)
			.await?;
			let models = fetch_models(
				&credentials.access,
				enterprise_domain.as_deref(),
				signal,
				0,
				0,
			)
			.await?;
			available_model_ids_extra(&mut credentials, models.available_model_ids);
			Ok(credentials)
		})
	}

	/// Derive the credential-specific proxy endpoint for each request.
	fn to_auth(&self, credential: &OAuthCredential) -> Result<ModelAuth, AuthError> {
		Ok(ModelAuth {
			api_key: Some(credential.access.clone()),
			headers: Default::default(),
			base_url: Some(get_base_url(
				Some(&credential.access),
				copilot_enterprise_domain(credential).as_deref(),
			)),
		})
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use serde_json::json;

	#[test]
	fn normalize_domain_accepts_bare_hosts_and_urls() {
		assert_eq!(
			normalize_domain("company.ghe.com").as_deref(),
			Some("company.ghe.com")
		);
		assert_eq!(
			normalize_domain("  company.ghe.com  ").as_deref(),
			Some("company.ghe.com")
		);
		assert_eq!(
			normalize_domain("https://company.ghe.com/prefix").as_deref(),
			Some("company.ghe.com")
		);
		assert_eq!(
			normalize_domain("http://10.0.0.1:8443").as_deref(),
			Some("10.0.0.1")
		);
		assert_eq!(normalize_domain(""), None);
		assert_eq!(normalize_domain("   "), None);
		assert_eq!(normalize_domain("bad host:name"), None);
	}

	#[test]
	fn urls_follow_the_enterprise_domain() {
		let urls = get_urls("github.com");
		assert_eq!(urls.device_code_url, "https://github.com/login/device/code");
		assert_eq!(
			urls.access_token_url,
			"https://github.com/login/oauth/access_token"
		);
		assert_eq!(
			urls.copilot_token_url,
			"https://api.github.com/copilot_internal/v2/token"
		);
	}

	#[test]
	fn base_url_extracts_proxy_endpoint_from_token() {
		assert_eq!(
			base_url_from_token("tid=test;exp=9;proxy-ep=proxy.individual.githubcopilot.com;"),
			Some("https://api.individual.githubcopilot.com".to_string())
		);
		assert_eq!(
			base_url_from_token("proxy-ep=proxy.business.githubcopilot.com"),
			Some("https://api.business.githubcopilot.com".to_string())
		);
		assert_eq!(base_url_from_token("tid=test"), None);
	}

	#[test]
	fn base_url_falls_back_to_enterprise_then_individual() {
		assert_eq!(
			get_base_url(None, Some("company.ghe.com")),
			"https://copilot-api.company.ghe.com"
		);
		assert_eq!(
			get_base_url(None, None),
			"https://api.individual.githubcopilot.com"
		);
		// Token parsing wins over the enterprise fallback.
		assert_eq!(
			get_base_url(
				Some("proxy-ep=proxy.individual.githubcopilot.com"),
				Some("company.ghe.com")
			),
			"https://api.individual.githubcopilot.com"
		);
	}

	#[test]
	fn http_date_parser_handles_imf_fixdate() {
		assert_eq!(
			parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT"),
			Some(784111777000)
		);
		assert_eq!(
			parse_http_date("Fri, 01 Jan 2027 00:00:00 GMT"),
			Some(1798761600000)
		);
		assert_eq!(parse_http_date("not a date"), None);
		assert_eq!(parse_http_date("Sun, 06 Nov 1994 08:49:37 PST"), None);
		assert_eq!(parse_http_date("Sun, 06 Xxx 1994 08:49:37 GMT"), None);
	}

	#[test]
	fn retry_delay_prefers_seconds_then_dates_and_gives_up_on_garbage() {
		let now = 1_000_000;
		assert_eq!(retry_delay_ms(Some("2"), 0, now), Some(2000));
		assert_eq!(retry_delay_ms(Some("0"), 0, now), Some(0));
		// Without a header the exponential backoff applies.
		assert_eq!(retry_delay_ms(None, 0, now), Some(500));
		assert_eq!(retry_delay_ms(None, 2, now), Some(2000));
		// Unparseable numbers try the HTTP-date form.
		assert_eq!(
			retry_delay_ms(Some("Sun, 06 Nov 1994 08:49:37 GMT"), 0, 784_111_000_000),
			Some(777_000)
		);
		assert_eq!(retry_delay_ms(Some("garbage"), 0, now), None);
		assert_eq!(retry_delay_ms(Some(""), 0, now), None);
	}

	#[test]
	fn parse_device_flow_validates_fields_and_uri_trust() {
		let ok = json!({
			"device_code": "dc",
			"user_code": "ABCD-EFGH",
			"verification_uri": "https://github.com/login/device",
			"interval": 5,
			"expires_in": 900
		});
		let device = parse_device_flow(Some(&ok)).unwrap();
		assert_eq!(device.device_code, "dc");
		assert_eq!(device.user_code, "ABCD-EFGH");
		assert_eq!(device.verification_uri, "https://github.com/login/device");
		assert_eq!(device.interval, Some(5.0));
		assert_eq!(device.expires_in, 900.0);

		// Interval is optional; expires_in is not.
		let mut no_interval = ok.clone();
		no_interval.as_object_mut().unwrap().remove("interval");
		assert_eq!(
			parse_device_flow(Some(&no_interval)).unwrap().interval,
			None
		);
		let mut no_expires = ok;
		no_expires.as_object_mut().unwrap().remove("expires_in");
		assert_eq!(
			parse_device_flow(Some(&no_expires))
				.unwrap_err()
				.to_string(),
			"Invalid device code response fields"
		);

		// A malicious enterprise server must not reach the browser opener.
		let mut untrusted = json!({
			"device_code": "dc",
			"user_code": "ABCD-EFGH",
			"verification_uri": "$(id>/tmp/pwned)",
			"expires_in": 900
		});
		assert_eq!(
			parse_device_flow(Some(&untrusted)).unwrap_err().to_string(),
			"Untrusted verification_uri in device code response"
		);
		untrusted["verification_uri"] = json!("ftp://github.com/login/device");
		assert_eq!(
			parse_device_flow(Some(&untrusted)).unwrap_err().to_string(),
			"Untrusted verification_uri in device code response"
		);

		assert_eq!(
			parse_device_flow(None).unwrap_err().to_string(),
			"Invalid device code response"
		);
	}

	#[test]
	fn device_token_poll_classifies_pending_slow_down_and_failures() {
		assert!(matches!(
			device_token_poll_result(Some(&json!({"access_token": "ghu"}))),
			DevicePollResult::Complete(value) if value == "ghu"
		));
		assert!(matches!(
			device_token_poll_result(Some(
				&json!({"error": "authorization_pending", "error_description": "p"})
			)),
			DevicePollResult::Pending
		));
		let slow_down = device_token_poll_result(Some(
			&json!({"error": "slow_down", "error_description": "s", "interval": 7}),
		));
		assert!(matches!(
			slow_down,
			DevicePollResult::SlowDown {
				interval_seconds: Some(7)
			}
		));
		let slow_down = device_token_poll_result(Some(&json!({"error": "slow_down"})));
		assert!(matches!(
			slow_down,
			DevicePollResult::SlowDown {
				interval_seconds: None
			}
		));
		let failed = device_token_poll_result(Some(&json!({"error": "unsupported_grant_type"})));
		assert_eq!(
			match failed {
				DevicePollResult::Failed { message } => message,
				_ => panic!("expected failure"),
			},
			"Device flow failed: unsupported_grant_type"
		);
		let failed =
			device_token_poll_result(Some(&json!({"error": "bad", "error_description": "why"})));
		assert_eq!(
			match failed {
				DevicePollResult::Failed { message } => message,
				_ => panic!("expected failure"),
			},
			"Device flow failed: bad: why"
		);
		let failed = device_token_poll_result(Some(&json!({})));
		assert_eq!(
			match failed {
				DevicePollResult::Failed { message } => message,
				_ => panic!("expected failure"),
			},
			"Invalid device token response"
		);
	}

	#[test]
	fn copilot_token_response_maps_fields_and_enterprise_url() {
		let credential = parse_copilot_token_response(
			Some(&json!({"token": "tid=test;exp=99;proxy-ep=proxy.individual.githubcopilot.com;", "expires_at": 9_999_999_999i64})),
			None,
		)
		.unwrap();
		assert_eq!(
			credential.access,
			"tid=test;exp=99;proxy-ep=proxy.individual.githubcopilot.com;"
		);
		assert_eq!(credential.expires, 9_999_999_999i64 * 1000 - 5 * 60 * 1000);
		assert!(!credential.extra.contains_key("enterpriseUrl"));

		let credential = parse_copilot_token_response(
			Some(&json!({"token": "t", "expires_at": 100})),
			Some("company.ghe.com"),
		)
		.unwrap();
		assert_eq!(
			credential
				.extra
				.get("enterpriseUrl")
				.and_then(|v| v.as_str()),
			Some("company.ghe.com")
		);

		assert_eq!(
			parse_copilot_token_response(None, None)
				.unwrap_err()
				.to_string(),
			"Invalid Copilot token response"
		);
		assert_eq!(
			parse_copilot_token_response(Some(&json!({"token": "t"})), None)
				.unwrap_err()
				.to_string(),
			"Invalid Copilot token response fields"
		);
		assert_eq!(
			parse_copilot_token_response(Some(&json!({"expires_at": 1})), None)
				.unwrap_err()
				.to_string(),
			"Invalid Copilot token response fields"
		);
	}

	#[test]
	fn catalog_parsing_filters_picker_disabled_and_tool_incapable_models() {
		// Picker catalog wins; disabled and hidden models drop out.
		let catalog = parse_catalog(
			Some(&json!({"data": [
				{"id": "picker-model", "model_picker_enabled": true, "capabilities": {"supports": {"tool_calls": true}}},
				{"id": "disabled-model", "model_picker_enabled": true, "policy": {"state": "disabled"}, "capabilities": {"supports": {"tool_calls": true}}},
				{"id": "hidden-model", "model_picker_enabled": false, "policy": {"state": "enabled"}, "capabilities": {"supports": {"tool_calls": true}}}
			]})),
			true,
		)
		.unwrap();
		assert_eq!(
			catalog.available_model_ids,
			vec!["picker-model".to_string()]
		);
		assert!(catalog.policy_model_ids.is_empty());

		// Policy fallback only applies on the Individual endpoint with an empty picker.
		let catalog = parse_catalog(
			Some(&json!({"data": [
				{"id": "enabled-model", "model_picker_enabled": false, "policy": {"state": "enabled"}, "capabilities": {"supports": {"tool_calls": true}}},
				{"id": "policy-disabled-model", "model_picker_enabled": false, "policy": {"state": "disabled"}},
				{"id": "tool-incapable-model", "model_picker_enabled": false, "policy": {"state": "enabled"}, "capabilities": {"supports": {"tool_calls": false}}}
			]})),
			true,
		)
		.unwrap();
		assert_eq!(
			catalog.available_model_ids,
			vec!["enabled-model".to_string()]
		);

		// Non-Individual accounts never fall back.
		let catalog = parse_catalog(
			Some(&json!({"data": [
				{"id": "enabled-model", "model_picker_enabled": false, "policy": {"state": "enabled"}, "capabilities": {"supports": {"tool_calls": true}}}
			]})),
			false,
		)
		.unwrap();
		assert!(catalog.available_model_ids.is_empty());

		// Missing or non-array data fails.
		assert_eq!(
			parse_catalog(Some(&json!({"error": "boom"})), true)
				.unwrap_err()
				.to_string(),
			"Invalid Copilot models response"
		);
		assert_eq!(
			parse_catalog(None, true).unwrap_err().to_string(),
			"Invalid Copilot models response"
		);

		// Entries without string ids are skipped.
		let catalog = parse_catalog(
			Some(&json!({"data": [{"id": 5}, {"capabilities": {}}]})),
			true,
		)
		.unwrap();
		assert!(catalog.available_model_ids.is_empty());
	}

	#[test]
	fn enterprise_domain_reads_the_flattened_extra_field() {
		let mut credential = OAuthCredential {
			access: "a".to_string(),
			refresh: "r".to_string(),
			expires: 0,
			extra: Default::default(),
		};
		assert_eq!(copilot_enterprise_domain(&credential), None);
		credential.extra.insert(
			"enterpriseUrl".to_string(),
			serde_json::Value::String("https://company.ghe.com/x".to_string()),
		);
		assert_eq!(
			copilot_enterprise_domain(&credential).as_deref(),
			Some("company.ghe.com")
		);
		credential.extra.insert(
			"enterpriseUrl".to_string(),
			serde_json::Value::String("".to_string()),
		);
		assert_eq!(copilot_enterprise_domain(&credential), None);
	}

	#[test]
	fn to_auth_derives_the_proxy_base_url() {
		let mut credential = OAuthCredential {
			access: "tid=1;proxy-ep=proxy.business.githubcopilot.com;".to_string(),
			refresh: "r".to_string(),
			expires: 0,
			extra: Default::default(),
		};
		let auth = GitHubCopilotOAuth.to_auth(&credential).unwrap();
		assert_eq!(
			auth.api_key.as_deref(),
			Some("tid=1;proxy-ep=proxy.business.githubcopilot.com;")
		);
		assert_eq!(
			auth.base_url.as_deref(),
			Some("https://api.business.githubcopilot.com")
		);
		assert!(auth.headers.is_empty());

		// Without a parsable token the enterprise/individual fallback applies.
		credential.access = "plain".to_string();
		let auth = GitHubCopilotOAuth.to_auth(&credential).unwrap();
		assert_eq!(
			auth.base_url.as_deref(),
			Some("https://api.individual.githubcopilot.com")
		);
	}
}
