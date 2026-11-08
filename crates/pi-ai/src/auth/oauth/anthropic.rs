//! Anthropic OAuth flow (Claude Pro/Max) — port of
//! `pi/packages/ai/src/auth/oauth/anthropic.ts`.

use futures::future::BoxFuture;

use crate::auth::callback::{CallbackOutcome, CallbackServerOptions, OAuthCallbackServer};
use crate::auth::pkce::generate_pkce;
use crate::auth::types::{
	AuthError, AuthEvent, AuthInteraction, AuthPrompt, AuthPromptKind, LoginOptions, ModelAuth,
	OAuthCredential, OAuthFlow, SelectOption,
};

/// Base64 of `9d1c250a-e61b-44d9-88ed-5944d1962f5e` (upstream obfuscates the
/// literal the same way).
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const CALLBACK_PORT: u16 = 53692;
const CALLBACK_PATH: &str = "/callback";
/// Preferred so the port can be forwarded into containers or over SSH;
/// Anthropic accepts any loopback port, so login falls back to a free port.
const CALLBACK_HOST_ENV: &str = "PI_OAUTH_CALLBACK_HOST";
const REDIRECT_URI: &str = "http://localhost:53692/callback";
const COPY_CODE_REDIRECT_URI: &str = "https://platform.claude.com/oauth/code/callback";
const BROWSER_LOGIN_METHOD: &str = "browser";
const COPY_CODE_LOGIN_METHOD: &str = "copy_code";
const SCOPES: &str = "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";

fn callback_host() -> String {
	std::env::var(CALLBACK_HOST_ENV)
		.ok()
		.filter(|value| !value.is_empty())
		.unwrap_or_else(|| "127.0.0.1".to_string())
}

/// Parse a pasted code / `code#state` / redirect URL (upstream
/// `parseAuthorizationInput`).
pub fn parse_authorization_input(input: &str) -> (Option<String>, Option<String>) {
	let value = input.trim();
	if value.is_empty() {
		return (None, None);
	}
	// URL branch (upstream `new URL(value)`): anything with a scheme and a
	// query string parses its code/state pairs.
	if (value.starts_with("http://") || value.starts_with("https://"))
		&& let Some((_, query)) = value.split_once('?')
	{
		let find = |key: &str| {
			crate::auth::callback::parse_query(query)
				.into_iter()
				.find(|(k, _)| k == key)
				.map(|(_, v)| v)
		};
		return (find("code"), find("state"));
	}
	if let Some((code, state)) = value.split_once('#') {
		return (Some(code.to_string()), Some(state.to_string()));
	}
	if value.contains("code=") {
		for part in value.split(['&', '?']) {
			if let Some(rest) = part.strip_prefix("code=") {
				return (Some(crate::auth::callback::percent_decode(rest)), None);
			}
		}
	}
	(Some(value.to_string()), None)
}

async fn exchange_authorization_code(
	code: &str,
	state: &str,
	verifier: &str,
	redirect_uri: &str,
	signal: tokio_util::sync::CancellationToken,
) -> Result<OAuthCredential, AuthError> {
	let body = serde_json::json!({
		"grant_type": "authorization_code",
		"client_id": CLIENT_ID,
		"code": code,
		"state": state,
		"redirect_uri": redirect_uri,
		"code_verifier": verifier,
	});
	let response = crate::auth::http::post_json(TOKEN_URL, &body, signal)
		.await
		.map_err(|error| {
			AuthError::Login(format!(
				"Token exchange request failed. url={TOKEN_URL}; redirect_uri={redirect_uri}; response_type=authorization_code; details={error}"
			))
		})?;
	let token: serde_json::Value = serde_json::from_str(&response).map_err(|error| {
		AuthError::Login(format!(
			"Token exchange returned invalid JSON. url={TOKEN_URL}; body={response}; details={error}"
		))
	})?;
	credential_from_token_response(&token)
}

/// Shared token-response → credential mapping (5-minute early expiry).
pub(crate) fn credential_from_token_response(
	token: &serde_json::Value,
) -> Result<OAuthCredential, AuthError> {
	let access = token
		.get("access_token")
		.and_then(|v| v.as_str())
		.ok_or_else(|| AuthError::Login(format!("token response missing access_token: {token}")))?;
	let refresh = token
		.get("refresh_token")
		.and_then(|v| v.as_str())
		.ok_or_else(|| {
			AuthError::Login(format!("token response missing refresh_token: {token}"))
		})?;
	let expires_in = token
		.get("expires_in")
		.and_then(|v| v.as_f64())
		.ok_or_else(|| AuthError::Login(format!("token response missing expires_in: {token}")))?;
	let now = std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map(|d| d.as_millis() as i64)
		.unwrap_or(0);
	Ok(OAuthCredential {
		access: access.to_string(),
		refresh: refresh.to_string(),
		expires: now + (expires_in * 1000.0) as i64 - 5 * 60 * 1000,
		extra: Default::default(),
	})
}

async fn login_browser(interaction: &dyn AuthInteraction) -> Result<OAuthCredential, AuthError> {
	let (verifier, challenge) = generate_pkce()?;
	// Without a callback server, login continues with the pasted redirect URL.
	let callback = OAuthCallbackServer::start(CallbackServerOptions {
		provider_name: "Anthropic".to_string(),
		host: callback_host(),
		port: CALLBACK_PORT,
		path: CALLBACK_PATH.to_string(),
		redirect_host: Some("localhost".to_string()),
		state: Some(verifier.clone()),
		signal: interaction.signal(),
		timeout_ms: None,
	})
	.or_else(|_| {
		OAuthCallbackServer::start(CallbackServerOptions {
			provider_name: "Anthropic".to_string(),
			host: callback_host(),
			port: 0,
			path: CALLBACK_PATH.to_string(),
			redirect_host: Some("localhost".to_string()),
			state: Some(verifier.clone()),
			signal: interaction.signal(),
			timeout_ms: None,
		})
	})
	.ok();
	let redirect_uri = callback
		.as_ref()
		.map(|server| server.redirect_uri.clone())
		.unwrap_or_else(|| REDIRECT_URI.to_string());

	let result: Result<OAuthCredential, AuthError> = async {
		let query = [
			("code", "true".to_string()),
			("client_id", CLIENT_ID.to_string()),
			("response_type", "code".to_string()),
			("redirect_uri", redirect_uri.clone()),
			("scope", SCOPES.to_string()),
			("code_challenge", challenge),
			("code_challenge_method", "S256".to_string()),
			("state", verifier.clone()),
		];
		let authorize = format!(
			"{AUTHORIZE_URL}?{}",
			crate::auth::http::encode_form(&query.iter().map(|(k, v)| (k.to_string(), v.clone())).collect::<Vec<_>>())
		);
		interaction.notify(AuthEvent::AuthUrl {
			url: authorize,
			instructions: Some(
				"Complete login in your browser. If the browser is on another machine, paste the final redirect URL here."
					.to_string(),
			),
		});

		let code = wait_for_callback_or_manual_input(
			interaction,
			callback.as_ref(),
			"Complete login in your browser, or paste the authorization code / redirect URL here:",
			&redirect_uri,
		)
		.await?;
		let (parsed_code, parsed_state) = match &code {
			CallbackOrManual::Callback(value) => (Some(value.clone()), None),
			CallbackOrManual::Manual(input) => {
				let (code, state) = parse_authorization_input(input);
				if state.as_deref().is_some_and(|state| state != verifier) {
					return Err(AuthError::Login("OAuth state mismatch".to_string()));
				}
				(code, state)
			}
		};
		let Some(code) = parsed_code else {
			return Err(AuthError::Login("Missing authorization code".to_string()));
		};
		interaction.notify(AuthEvent::Progress {
			message: "Exchanging authorization code for tokens...".to_string(),
		});
		exchange_authorization_code(
			&code,
			parsed_state.as_deref().unwrap_or(&verifier),
			&verifier,
			&redirect_uri,
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

async fn login_copy_code(interaction: &dyn AuthInteraction) -> Result<OAuthCredential, AuthError> {
	let (verifier, challenge) = generate_pkce()?;
	let query = [
		("code", "true".to_string()),
		("client_id", CLIENT_ID.to_string()),
		("response_type", "code".to_string()),
		("redirect_uri", COPY_CODE_REDIRECT_URI.to_string()),
		("scope", SCOPES.to_string()),
		("code_challenge", challenge),
		("code_challenge_method", "S256".to_string()),
		("state", verifier.clone()),
	];
	let authorize = format!(
		"{AUTHORIZE_URL}?{}",
		crate::auth::http::encode_form(
			&query
				.iter()
				.map(|(k, v)| (k.to_string(), v.clone()))
				.collect::<Vec<_>>()
		)
	);
	interaction.notify(AuthEvent::AuthUrl {
		url: authorize,
		instructions: Some(
			"Complete login in your browser, then copy the code Anthropic shows and paste it here."
				.to_string(),
		),
	});
	let input = interaction
		.prompt(AuthPrompt {
			kind: AuthPromptKind::ManualCode {
				message: "Paste the code Anthropic shows after you sign in:".to_string(),
				placeholder: Some("code#state".to_string()),
			},
			signal: interaction.signal(),
		})
		.await?;
	let (code, state) = parse_authorization_input(&input);
	if state.as_deref().is_some_and(|state| state != verifier) {
		return Err(AuthError::Login("OAuth state mismatch".to_string()));
	}
	let Some(code) = code else {
		return Err(AuthError::Login("Missing authorization code".to_string()));
	};
	interaction.notify(AuthEvent::Progress {
		message: "Exchanging authorization code for tokens...".to_string(),
	});
	exchange_authorization_code(
		&code,
		state.as_deref().unwrap_or(&verifier),
		&verifier,
		COPY_CODE_REDIRECT_URI,
		interaction.signal(),
	)
	.await
}

/// How [`wait_for_callback_or_manual_input`] resolved: the browser came back
/// with a code, or the user pasted one.
pub enum CallbackOrManual {
	Callback(String),
	Manual(String),
}

/// Wait for the browser callback, or for the user to paste the code or
/// redirect URL when the browser cannot reach the loopback server (upstream
/// `waitForCallbackOrManualInput`).
async fn wait_for_callback_or_manual_input(
	interaction: &dyn AuthInteraction,
	callback: Option<&OAuthCallbackServer>,
	message: &str,
	placeholder: &str,
) -> Result<CallbackOrManual, AuthError> {
	let manual_signal = tokio_util::sync::CancellationToken::new();
	let manual_prompt = interaction.prompt(AuthPrompt {
		kind: AuthPromptKind::ManualCode {
			message: message.to_string(),
			placeholder: Some(placeholder.to_string()),
		},
		signal: manual_signal.clone(),
	});
	tokio::pin!(manual_prompt);

	let outcome = if let Some(callback) = callback {
		let wait = callback.wait();
		tokio::pin!(wait);
		tokio::select! {
			outcome = &mut wait => outcome,
			result = &mut manual_prompt => {
				callback.cancel();
				manual_signal.cancel();
				return result.map(CallbackOrManual::Manual);
			}
		}
	} else {
		// Without a callback server only the manual prompt is used.
		return manual_prompt.await.map(CallbackOrManual::Manual);
	};
	manual_signal.cancel();
	match outcome {
		CallbackOutcome::Code { code: value, .. } => Ok(CallbackOrManual::Callback(value)),
		CallbackOutcome::ProviderError(message) => Err(AuthError::Login(message)),
		CallbackOutcome::Cancelled => manual_prompt.await.map(CallbackOrManual::Manual),
	}
}

/// Refresh an Anthropic OAuth token.
async fn refresh_token(
	refresh_token: &str,
	signal: tokio_util::sync::CancellationToken,
) -> Result<OAuthCredential, AuthError> {
	let body = serde_json::json!({
		"grant_type": "refresh_token",
		"client_id": CLIENT_ID,
		"refresh_token": refresh_token,
	});
	let response = crate::auth::http::post_json(TOKEN_URL, &body, signal)
		.await
		.map_err(|error| {
			AuthError::Login(format!(
				"Anthropic token refresh request failed. url={TOKEN_URL}; details={error}"
			))
		})?;
	let token: serde_json::Value = serde_json::from_str(&response).map_err(|error| {
		AuthError::Login(format!(
			"Anthropic token refresh returned invalid JSON. url={TOKEN_URL}; body={response}; details={error}"
		))
	})?;
	credential_from_token_response(&token)
}

pub struct AnthropicOAuth;

impl OAuthFlow for AnthropicOAuth {
	fn name(&self) -> &str {
		"Anthropic (Claude Pro/Max)"
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
			let method = interaction
				.prompt(AuthPrompt {
					kind: AuthPromptKind::Select {
						message: "Select Anthropic login method:".to_string(),
						options: vec![
							SelectOption {
								id: BROWSER_LOGIN_METHOD.to_string(),
								label: "Browser login (default)".to_string(),
								description: None,
							},
							SelectOption {
								id: COPY_CODE_LOGIN_METHOD.to_string(),
								label: "Copy code login (headless)".to_string(),
								description: None,
							},
						],
					},
					signal: interaction.signal(),
				})
				.await?;
			match method.as_str() {
				COPY_CODE_LOGIN_METHOD => login_copy_code(interaction).await,
				BROWSER_LOGIN_METHOD => login_browser(interaction).await,
				other => Err(AuthError::Login(format!(
					"Unknown Anthropic login method: {other}"
				))),
			}
		})
	}

	fn refresh<'a>(
		&'a self,
		credential: &'a OAuthCredential,
	) -> BoxFuture<'a, Result<OAuthCredential, AuthError>> {
		Box::pin(async move {
			refresh_token(
				&credential.refresh,
				tokio_util::sync::CancellationToken::new(),
			)
			.await
		})
	}

	fn to_auth(&self, credential: &OAuthCredential) -> Result<ModelAuth, AuthError> {
		Ok(ModelAuth {
			api_key: Some(credential.access.clone()),
			..Default::default()
		})
	}
}
