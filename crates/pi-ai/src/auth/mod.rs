//! Provider auth — port of `pi/packages/ai/src/auth/*`.
//!
//! Wire format: `auth.json` maps provider ids to one type-tagged credential
//! (`{type:"api_key", key?, env?}` or `{type:"oauth", access, refresh,
//! expires, ...extra}`), written 0600 under a lock file. Login flows are
//! provider-owned ([`OAuthFlow`]); refresh runs under the store lock
//! ([`resolve::refresh_stored_oauth_credential`]) so concurrent requests
//! never double-refresh a rotated token.
//!
//! Declared differences from the TS original:
//! - `radius`/`llama.cpp` flows have no pi-rs provider counterpart and are
//!   not ported; the bundled/lazy loader dance is a bundler concern.
//! - Ambient api-key resolution (AWS profiles, ADC files) stays inside the
//!   existing provider adapters; this module only owns stored credentials.
//! - The mkdir-based lock replaces `proper-lockfile` (same 30s stale/deadline
//!   parameters; no Windows EPERM ladder).

pub mod callback;
pub mod device;
pub mod pkce;
pub mod resolve;
pub mod store;
pub mod types;

pub mod oauth;

pub use callback::{CallbackOutcome, CallbackServerOptions, OAuthCallbackServer};
pub use device::{DevicePollResult, poll_oauth_device_code_flow};
pub use resolve::{
	DEFAULT_OAUTH_MINIMUM_VALIDITY_MS, refresh_stored_oauth_credential, resolve_stored_auth,
};
pub use store::{AuthStorage, InMemoryAuthStorage};
pub use types::{
	ApiKeyCredential, AuthError, AuthEvent, AuthInteraction, AuthPrompt, AuthPromptKind,
	AuthResult, Credential, CredentialInfo, LoginOptions, ModelAuth, OAuthCredential, OAuthFlow,
	SelectOption,
};

/// The provider ids with an OAuth login flow (`oauth_provider_ids`), in
/// catalog order like upstream's provider table.
pub fn oauth_flow_for(provider_id: &str) -> Option<std::sync::Arc<dyn OAuthFlow>> {
	match provider_id {
		"anthropic" => Some(std::sync::Arc::new(oauth::anthropic::AnthropicOAuth)),
		"openai" => Some(std::sync::Arc::new(
			oauth::openai_chatgpt::OpenAIChatGptOAuth,
		)),
		"openai-codex" => Some(std::sync::Arc::new(oauth::openai_codex::OpenAICodexOAuth)),
		"github-copilot" => Some(std::sync::Arc::new(
			oauth::github_copilot::GitHubCopilotOAuth,
		)),
		"openrouter" => Some(std::sync::Arc::new(oauth::openrouter::OpenRouterOAuth)),
		"kimi-coding" => Some(std::sync::Arc::new(oauth::kimi_coding::KimiCodingOAuth)),
		"xai" => Some(std::sync::Arc::new(oauth::xai::XaiOAuth)),
		"meta" => Some(std::sync::Arc::new(oauth::meta::MetaOAuth)),
		_ => None,
	}
}

/// Every provider id [`oauth_flow_for`] knows.
pub fn oauth_provider_ids() -> &'static [&'static str] {
	&[
		"anthropic",
		"openai",
		"openai-codex",
		"github-copilot",
		"openrouter",
		"kimi-coding",
		"xai",
		"meta",
	]
}

/// Shared token-endpoint HTTP helpers for the login flows (upstream
/// `postJson`/form posts inside each flow module): 30s wall clock, text body
/// surfaced on non-2xx.
pub(crate) mod http {
	use super::types::AuthError;

	const HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

	fn client() -> reqwest::Client {
		reqwest::Client::builder()
			.timeout(HTTP_TIMEOUT)
			.build()
			.expect("reqwest client builds")
	}

	pub(crate) async fn post_json(
		url: &str,
		body: &serde_json::Value,
		signal: tokio_util::sync::CancellationToken,
	) -> Result<String, AuthError> {
		let request = client()
			.post(url)
			.header("Content-Type", "application/json")
			.header("Accept", "application/json")
			.body(body.to_string());
		let response = send_with_cancellation(request, signal).await?;
		let status = response.status();
		let text = response
			.text()
			.await
			.map_err(|e| AuthError::Login(e.to_string()))?;
		if !status.is_success() {
			return Err(AuthError::Login(format!(
				"HTTP request failed. status={status}; url={url}; body={text}"
			)));
		}
		Ok(text)
	}

	#[allow(dead_code)]
	pub(crate) async fn post_form(
		url: &str,
		form: &[(String, String)],
		signal: tokio_util::sync::CancellationToken,
	) -> Result<reqwest::Response, AuthError> {
		let request = client()
			.post(url)
			.header("Content-Type", "application/x-www-form-urlencoded")
			.body(encode_form(form));
		send_with_cancellation(request, signal).await
	}

	pub(crate) fn encode_form(form: &[(String, String)]) -> String {
		form.iter()
			.map(|(key, value)| format!("{}={}", percent_encode(key), percent_encode(value)))
			.collect::<Vec<_>>()
			.join("&")
	}

	fn percent_encode(value: &str) -> String {
		let mut out = String::with_capacity(value.len());
		for byte in value.as_bytes() {
			match byte {
				b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
					out.push(*byte as char)
				}
				other => out.push_str(&format!("%{other:02X}")),
			}
		}
		out
	}

	/// Drive a request to completion, mapping login cancellation onto the
	/// upstream "Login cancelled" message.
	pub(crate) async fn send_with_cancellation(
		request: reqwest::RequestBuilder,
		signal: tokio_util::sync::CancellationToken,
	) -> Result<reqwest::Response, AuthError> {
		tokio::select! {
			response = request.send() => response.map_err(|error| {
				if signal.is_cancelled() {
					AuthError::Login("Login cancelled".to_string())
				} else {
					AuthError::Login(error.to_string())
				}
			}),
			() = signal.cancelled() => Err(AuthError::Login("Login cancelled".to_string())),
		}
	}
}
