//! Provider OAuth login flows — ports of `pi/packages/ai/src/auth/oauth/*`.
//! Each module implements [`crate::auth::OAuthFlow`]; the provider-id → flow
//! mapping lives in [`crate::auth::oauth_flow_for`].

pub mod anthropic;
pub mod github_copilot;
pub mod kimi_coding;
pub mod meta;
pub mod openai_chatgpt;
pub mod openai_codex;
pub mod openrouter;
pub mod xai;

use crate::auth::callback::{CallbackOutcome, OAuthCallbackServer};
use crate::auth::types::{AuthError, AuthInteraction, AuthPrompt, AuthPromptKind};

/// Epoch milliseconds (JS `Date.now()`).
pub(crate) fn now_ms() -> i64 {
	std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map(|d| d.as_millis() as i64)
		.unwrap_or(0)
}

/// Send one HTTP request with explicit headers and an optional body (the
/// upstream flows build their `fetch` calls with per-flow headers; the shared
/// [`crate::auth::http`] helpers only cover the header-less shapes). Carries
/// the same 30s wall clock as `crate::auth::http` and maps login cancellation
/// onto the upstream "Login cancelled" message.
pub(crate) async fn send_request(
	method: reqwest::Method,
	url: &str,
	headers: &[(&str, &str)],
	body: Option<String>,
	signal: tokio_util::sync::CancellationToken,
) -> Result<reqwest::Response, AuthError> {
	let client = reqwest::Client::builder()
		.timeout(std::time::Duration::from_secs(30))
		.build()
		.expect("reqwest client builds");
	let mut request = client.request(method, url);
	for (name, value) in headers {
		request = request.header(*name, *value);
	}
	if let Some(body) = body {
		request = request.body(body);
	}
	crate::auth::http::send_with_cancellation(request, signal).await
}

/// POST `application/x-www-form-urlencoded` fields in the given order.
pub(crate) async fn post_form_fields(
	url: &str,
	fields: &[(&str, &str)],
	headers: &[(&str, &str)],
	signal: tokio_util::sync::CancellationToken,
) -> Result<reqwest::Response, AuthError> {
	let form = fields
		.iter()
		.map(|(key, value)| (key.to_string(), value.to_string()))
		.collect::<Vec<_>>();
	send_request(
		reqwest::Method::POST,
		url,
		headers,
		Some(crate::auth::http::encode_form(&form)),
		signal,
	)
	.await
}

/// Wait for the browser callback, or for the user to paste the code or
/// redirect URL when the browser cannot reach the loopback server (upstream
/// `waitForCallbackOrManualInput` in `callback-server.ts`, also mirrored in
/// `anthropic.rs`). `Ok(Ok(code))` = the browser callback delivered `code`;
/// `Ok(Err(input))` = the manual prompt answered with `input`; `Err` = hard
/// login failure.
pub(crate) async fn wait_for_callback_or_manual_input(
	interaction: &dyn AuthInteraction,
	callback: Option<&OAuthCallbackServer>,
	message: &str,
	placeholder: &str,
) -> Result<Result<String, String>, AuthError> {
	let manual_signal = tokio_util::sync::CancellationToken::new();
	let manual_prompt = {
		let signal = manual_signal.clone();
		let message = message.to_string();
		async move {
			interaction
				.prompt(AuthPrompt {
					kind: AuthPromptKind::ManualCode {
						message,
						placeholder: Some(placeholder.to_string()),
					},
					signal,
				})
				.await
		}
	};
	tokio::pin!(manual_prompt);

	let outcome = if let Some(callback) = callback {
		let wait = callback.wait();
		tokio::pin!(wait);
		tokio::select! {
			outcome = &mut wait => outcome,
			result = &mut manual_prompt => {
				callback.cancel();
				return match result {
					// The manual prompt answered; the pasted input travels as
					// the inner Err (callback codes use the inner Ok).
					Ok(input) => Ok(Err(input)),
					Err(error) => Err(error),
				};
			}
		}
	} else {
		// Without a callback server only the manual prompt is used.
		return manual_prompt.await.map(Err);
	};
	manual_signal.cancel();
	match outcome {
		CallbackOutcome::Code(value) => Ok(Ok(value)),
		CallbackOutcome::ProviderError(message) => Err(AuthError::Login(message)),
		CallbackOutcome::Cancelled => {
			// Fall through to the manual prompt.
			manual_prompt.await.map(Err)
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::auth::types::{AuthEvent, AuthResult};
	use futures::future::BoxFuture;

	/// Canned manual-prompt answer (`Err` = the prompt itself failed).
	struct StubInteraction {
		signal: tokio_util::sync::CancellationToken,
		answer: Result<String, ()>,
	}

	impl AuthInteraction for StubInteraction {
		fn signal(&self) -> tokio_util::sync::CancellationToken {
			self.signal.clone()
		}

		fn prompt(&self, _prompt: AuthPrompt) -> BoxFuture<'_, AuthResult<String>> {
			let answer = match &self.answer {
				Ok(input) => Ok(input.clone()),
				Err(()) => Err(AuthError::Login("Login cancelled".to_string())),
			};
			Box::pin(async move { answer })
		}

		fn notify(&self, _event: AuthEvent) {}
	}

	#[tokio::test]
	async fn manual_only_prompt_returns_the_pasted_input() {
		let interaction = StubInteraction {
			signal: tokio_util::sync::CancellationToken::new(),
			answer: Ok("code#state".to_string()),
		};
		let result = wait_for_callback_or_manual_input(
			&interaction,
			None,
			"paste here:",
			"http://localhost/x",
		)
		.await
		.unwrap();
		assert_eq!(result, Err("code#state".to_string()));
	}

	#[tokio::test]
	async fn manual_prompt_failure_is_a_hard_login_error() {
		let interaction = StubInteraction {
			signal: tokio_util::sync::CancellationToken::new(),
			answer: Err(()),
		};
		let error = wait_for_callback_or_manual_input(
			&interaction,
			None,
			"paste here:",
			"http://localhost/x",
		)
		.await
		.unwrap_err();
		assert_eq!(error.to_string(), "Login cancelled");
	}
}
