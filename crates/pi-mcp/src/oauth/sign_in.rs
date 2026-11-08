//! Interactive MCP OAuth sign-in — port of the orchestration in
//! `pi/packages/coding-agent/src/extensions/mcp/oauth.ts`
//! (`signInMcpServer` and its helpers). The flow primitives themselves live
//! in [`super::flow`] and [`super::provider`]; this module adds the
//! sign-in-level behavior: loopback callback listening with the upstream
//! port-reuse ladder, the manual pasted-redirect-URL fallback (SSH-remote
//! terminals), the keep-client decision on a re-sign-in, and step-up scope
//! merging from a stored challenge.
//!
//! UI is behind [`SignInPrompt`]: the CLI prompt prints the URL and reads a
//! pasted redirect URL from stdin, the TUI prompt emits a notice and only
//! waits for the browser. Opening the browser belongs to the prompt —
//! upstream's `showAuthorizationUrl` implementations call `openBrowser`.

use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use super::flow::{self, FlowOptions, FlowResult};
use super::provider::{McpOAuthProvider, McpOAuthProviderOptions, OAuthSettings};
use super::types::{Challenge, ClientInformation, OAuthState};

/// Bind address of the loopback callback (upstream `CALLBACK_HOST`).
const CALLBACK_HOST: &str = "127.0.0.1";
/// Redirect path (upstream `CALLBACK_PATH`).
const CALLBACK_PATH: &str = "/callback";

/// What sign-in talks to the user through (upstream `McpSignInPrompt`).
#[async_trait::async_trait]
pub trait SignInPrompt: Send + Sync {
	/// Show the authorization URL to the user and open it in a browser.
	fn show_authorization_url(&self, url: &reqwest::Url);
	/// Ask for the redirect URL from the browser address bar, for when the
	/// browser cannot reach the loopback callback (for example over SSH).
	/// Cancelled once the callback arrives or the sign-in is cancelled;
	/// `None` means the user (or the cancellation) gave up.
	async fn prompt_for_redirect_url(&self, cancel: CancellationToken) -> Option<String>;
}

/// Why a sign-in failed (upstream throws `McpSignInCancelledError` or a
/// plain `Error`).
#[derive(Debug)]
pub enum SignInError {
	/// The sign-in was cancelled or its deadline passed.
	Cancelled,
	/// The OAuth flow itself failed.
	Flow(super::OAuthError),
	/// A sign-in-level validation failed; the message matches upstream.
	Invalid(String),
}

impl std::fmt::Display for SignInError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			SignInError::Cancelled => f.write_str("Sign-in cancelled"),
			SignInError::Flow(error) => write!(f, "{error}"),
			SignInError::Invalid(message) => f.write_str(message),
		}
	}
}

impl std::error::Error for SignInError {}

impl From<super::OAuthError> for SignInError {
	fn from(error: super::OAuthError) -> Self {
		SignInError::Flow(error)
	}
}

/// Where the loopback callback server listens and the redirect URI it
/// serves (upstream private `CallbackSettings`).
struct CallbackSettings {
	/// Address to listen on.
	host: String,
	/// Host name in the redirect URI.
	redirect_host: String,
	port: Option<u16>,
	path: String,
	/// The exact redirect URI, when the port is fixed.
	fixed_redirect_url: Option<String>,
}

fn callback_settings(settings: &OAuthSettings) -> CallbackSettings {
	// Upstream builds a URL from `settings.callbackUrl` or the default and
	// reads the pieces back out.
	let (url, raw) = match &settings.redirect_url {
		Some(raw) => match reqwest::Url::parse(raw) {
			Ok(url) => (Some(url), Some(raw.clone())),
			// An unparseable configured URL falls back to the default, like
			// upstream's `new URL(...)` throw surfacing later — pi-rs keeps
			// the flow alive and lets the authorization request fail with
			// the real redirect mismatch.
			Err(_) => (None, Some(raw.clone())),
		},
		None => (None, None),
	};
	let address = url
		.as_ref()
		.and_then(|url| url.host_str())
		.map(str::to_string)
		.unwrap_or_else(|| CALLBACK_HOST.to_string());
	let path = url
		.as_ref()
		.map(|url| url.path().to_string())
		.filter(|path| !path.is_empty())
		.unwrap_or_else(|| CALLBACK_PATH.to_string());
	let port = url
		.as_ref()
		.and_then(|url| url.port())
		.or(settings.callback_port);
	let fixed_redirect_url = if url.as_ref().is_some_and(|url| url.port().is_some()) {
		raw
	} else if let Some(port) = port {
		url.as_ref()
			.map(|url| {
				let mut url = url.clone();
				url.set_port(Some(port)).ok();
				url.to_string()
			})
			.or_else(|| Some(format!("http://{CALLBACK_HOST}:{port}{path}")))
	} else {
		None
	};
	CallbackSettings {
		// `localhost` is served on 127.0.0.1; browsers fall back to it when
		// ::1 refuses.
		host: if address == "localhost" {
			CALLBACK_HOST.to_string()
		} else {
			address.clone()
		},
		redirect_host: address,
		port,
		path,
		fixed_redirect_url,
	}
}

/// Scopes of both lists, each once (upstream `mergeScopes`).
pub(crate) fn merge_scopes(scopes: &[Option<&str>]) -> Option<String> {
	let merged: Vec<&str> = scopes
		.iter()
		.flat_map(|scope| scope.iter().flat_map(|scope| scope.split_whitespace()))
		.collect();
	// Deduplicate in order, like `new Set`.
	let mut seen: Vec<&str> = Vec::new();
	for scope in merged {
		if !seen.contains(&scope) {
			seen.push(scope);
		}
	}
	if seen.is_empty() {
		None
	} else {
		Some(seen.join(" "))
	}
}

/// Redirect URIs of a stored client (upstream `registeredRedirectUrls`).
fn registered_redirect_urls(client: Option<&ClientInformation>) -> Vec<String> {
	client
		.and_then(|client| client.redirect_uris.clone())
		.unwrap_or_default()
}

/// Everything one sign-in needs (upstream the `signInMcpServer` argument
/// object).
pub struct SignInOptions {
	/// The exact MCP server URL the flow authorizes.
	pub server_url: String,
	/// The server's slice of the credential store.
	pub store: Arc<dyn super::provider::OAuthStateStore>,
	/// Resolved OAuth client settings from the server config.
	pub settings: OAuthSettings,
	/// The server's last challenge, when a rejected request asked for
	/// sign-in (upstream `connection.challenge`).
	pub challenge: Option<Challenge>,
	/// Cancels the sign-in at any step (upstream `AbortSignal.timeout`).
	pub cancel: CancellationToken,
}

/// Sign in to an MCP server. Uses the stored refresh token when possible;
/// otherwise runs the browser authorization code flow. Tokens are saved to
/// the store. Cancelling stops the sign-in at any step with
/// [`SignInError::Cancelled`]; requests to the authorization server are
/// time-limited by the provider's client.
pub async fn sign_in(options: SignInOptions, prompt: &dyn SignInPrompt) -> Result<(), SignInError> {
	if options.cancel.is_cancelled() {
		return Err(SignInError::Cancelled);
	}
	let SignInOptions {
		server_url,
		store,
		settings,
		challenge,
		cancel,
	} = options;
	let stored = store.load().await.map_err(SignInError::Invalid)?;
	let step_up = challenge
		.as_ref()
		.is_some_and(|challenge| challenge.error.as_deref() == Some("insufficient_scope"));
	// The granted scope of the loaded snapshot, before the keep-client
	// rewrite below may clear the tokens (upstream reads the same snapshot).
	let granted_scope = stored
		.as_ref()
		.and_then(|state| state.tokens.as_ref())
		.and_then(|tokens| tokens.scope.clone());
	let callback_options = callback_settings(&settings);
	// Reuse the port of the registered redirect URI so the registered client
	// stays valid.
	let registered = registered_redirect_urls(
		stored
			.as_ref()
			.and_then(|state| state.client_information.as_ref()),
	);
	let preferred_port = callback_options.port.or_else(|| {
		registered
			.first()
			.and_then(|url| reqwest::Url::parse(url).ok().and_then(|url| url.port()))
	});
	// A configured URI with a port is served on the configured address; the
	// free-port fallback only applies to a port learned from a registration.
	let callback = match pi_ai::auth::callback::OAuthCallbackServer::start(
		pi_ai::auth::callback::CallbackServerOptions {
			provider_name: "MCP".to_string(),
			host: callback_options.host.clone(),
			port: preferred_port.unwrap_or(0),
			path: callback_options.path.clone(),
			redirect_host: Some(callback_options.redirect_host.clone()),
			state: None,
			signal: cancel.clone(),
			timeout_ms: None,
		},
	) {
		Ok(server) => server,
		Err(error) => {
			if preferred_port.is_none() {
				return Err(SignInError::Invalid(error.to_string()));
			}
			pi_ai::auth::callback::OAuthCallbackServer::start(
				pi_ai::auth::callback::CallbackServerOptions {
					provider_name: "MCP".to_string(),
					host: callback_options.host.clone(),
					port: 0,
					path: callback_options.path.clone(),
					redirect_host: Some(callback_options.redirect_host.clone()),
					state: None,
					signal: cancel.clone(),
					timeout_ms: None,
				},
			)
			.map_err(|error| SignInError::Invalid(error.to_string()))?
		}
	};
	let result = sign_in_with_callback(
		&server_url,
		&store,
		&settings,
		challenge.as_ref(),
		step_up,
		&callback_options,
		&callback,
		stored,
		granted_scope,
		prompt,
		&cancel,
	)
	.await;
	callback.close();
	result
}

/// The body of [`sign_in`] after the callback server is up, so `close`
/// always runs (upstream's `finally`).
#[allow(clippy::too_many_arguments)]
async fn sign_in_with_callback(
	server_url: &str,
	store: &Arc<dyn super::provider::OAuthStateStore>,
	settings: &OAuthSettings,
	challenge: Option<&Challenge>,
	step_up: bool,
	callback_options: &CallbackSettings,
	callback: &pi_ai::auth::callback::OAuthCallbackServer,
	stored: Option<OAuthState>,
	granted_scope: Option<String>,
	prompt: &dyn SignInPrompt,
	cancel: &CancellationToken,
) -> Result<(), SignInError> {
	let redirect_url = callback_options
		.fixed_redirect_url
		.clone()
		.unwrap_or_else(|| callback.redirect_uri.clone());
	if let Some(mut next) = stored {
		// Every sign-in gets a fresh `state` parameter.
		next.oauth_state = None;
		// A registered client cannot use another redirect URI, and its
		// tokens belong to it.
		let keep_client = settings.client_id.is_some()
			|| registered_redirect_urls(next.client_information.as_ref())
				.iter()
				.any(|url| url == &redirect_url);
		if !keep_client {
			next.client_information = None;
			next.tokens = None;
			next.tokens_expire_at = None;
		}
		store.save(next).await.map_err(SignInError::Invalid)?;
	}

	// The flow calls the hook exactly when it needs a browser; the URL also
	// carries the redirect URI it picked (which may be specific to the MCP
	// server).
	let authorization_url = Arc::new(tokio::sync::Mutex::new(None::<reqwest::Url>));
	let url_slot = Arc::clone(&authorization_url);
	let provider = McpOAuthProvider::new(McpOAuthProviderOptions {
		server_url: server_url.to_string(),
		redirect_url: redirect_url.clone(),
		client_metadata: super::flow::ClientMetadata {
			client_name: Some(
				settings
					.client_name
					.clone()
					.unwrap_or_else(|| "pi-rs".to_string()),
			),
			..Default::default()
		},
		client_id: settings.client_id.clone(),
		client_secret: settings.client_secret.clone(),
		store: Arc::clone(store),
		on_redirect: Some(Arc::new(move |url: &reqwest::Url| {
			if let Ok(mut slot) = url_slot.try_lock() {
				*slot = Some(url.clone());
			}
		})),
		client_metadata_document: None,
	});
	// A server asking for more scope gets it on top of the configured scope
	// and, since the challenge may list only the missing scopes, on top of
	// the scope granted so far.
	let challenged_scope = if step_up {
		flow::step_up_scope(
			granted_scope.as_deref(),
			challenge.and_then(|challenge| challenge.scope.as_deref()),
		)
	} else {
		challenge.and_then(|challenge| challenge.scope.clone())
	};
	let scope = merge_scopes(&[settings.scope.as_deref(), challenged_scope.as_deref()]);
	let flow_options = |authorization_code: Option<String>, iss: Option<String>| FlowOptions {
		server_url: server_url.to_string(),
		authorization_code,
		iss,
		scope: scope.clone(),
		resource_metadata_url: challenge
			.and_then(|challenge| challenge.resource_metadata_url.clone()),
		authorization_server_metadata_url: settings.auth_server_metadata_url.clone(),
		// A refresh keeps the granted scope; a server asking for more needs
		// the browser flow.
		skip_refresh: step_up,
	};
	if flow::authorize(&provider, flow_options(None, None)).await? == FlowResult::Authorized {
		return Ok(());
	}
	let authorization_url = authorization_url.lock().await.clone();
	let Some(authorization_url) = authorization_url else {
		return Err(SignInError::Invalid(
			"OAuth flow did not produce an authorization URL".to_string(),
		));
	};

	let state = provider.state().await?;
	// The flow picks the redirect URI, which may be specific to the MCP
	// server.
	let authorization_redirect_url = authorization_url
		.query_pairs()
		.find(|(key, _)| key == "redirect_uri")
		.map(|(_, value)| value.into_owned())
		.and_then(|value| reqwest::Url::parse(&value).ok())
		.unwrap_or_else(|| reqwest::Url::parse(&redirect_url).expect("redirect url parses"));
	prompt.show_authorization_url(&authorization_url);
	let response = wait_for_authorization_response(
		callback,
		&state,
		&authorization_redirect_url,
		prompt,
		cancel,
	)
	.await?;
	match flow::authorize(&provider, flow_options(Some(response.code), response.iss)).await {
		Ok(FlowResult::Authorized) | Ok(FlowResult::Redirect) => {
			// The parked browser connection gets the completion page
			// (upstream renders it from the flow result); the paste path has
			// no parked socket and this no-ops.
			callback.complete_with(
				true,
				pi_ai::auth::callback::oauth_success_html(
					"Signed in to the MCP server. You may now close this page.",
				),
			);
			Ok(())
		}
		Err(error) => {
			callback.complete_with(
				false,
				pi_ai::auth::callback::oauth_error_html(
					"MCP sign-in failed.",
					Some(&error.to_string()),
				),
			);
			Err(error.into())
		}
	}
}

#[derive(Debug)]
struct AuthorizationResponse {
	code: String,
	iss: Option<String>,
}

/// Validate a pasted redirect URL (upstream `responseFromRedirectUrl`).
fn response_from_redirect_url(
	input: &str,
	state: &str,
	redirect_url: &reqwest::Url,
) -> Result<AuthorizationResponse, SignInError> {
	let url = match reqwest::Url::parse(input.trim()) {
		Ok(url) => url,
		Err(_) => {
			return Err(SignInError::Invalid(
				"Expected the full redirect URL from the browser address bar".to_string(),
			));
		}
	};
	// A server-specific redirect URI tells authorization servers apart, so
	// it must match exactly.
	if url.origin() != redirect_url.origin() || url.path() != redirect_url.path() {
		return Err(SignInError::Invalid(
			"The redirect URL does not match this sign-in's redirect URI".to_string(),
		));
	}
	let params: Vec<(String, String)> = url
		.query_pairs()
		.map(|(k, v)| (k.into_owned(), v.into_owned()))
		.collect();
	if let Some((_, error)) = params.iter().find(|(key, _)| key == "error") {
		let description = params
			.iter()
			.find(|(key, _)| key == "error_description")
			.map(|(_, value)| value.clone())
			.unwrap_or_else(|| error.clone());
		return Err(SignInError::Invalid(description));
	}
	let matches_state = params
		.iter()
		.find(|(key, _)| key == "state")
		.is_some_and(|(_, value)| value == state);
	if !matches_state {
		return Err(SignInError::Invalid(
			"The redirect URL belongs to a different sign-in".to_string(),
		));
	}
	let Some((_, code)) = params.iter().find(|(key, _)| key == "code") else {
		return Err(SignInError::Invalid(
			"The redirect URL does not contain an authorization code".to_string(),
		));
	};
	Ok(AuthorizationResponse {
		code: code.clone(),
		iss: params
			.iter()
			.find(|(key, _)| key == "iss")
			.map(|(_, value)| value.clone()),
	})
}

/// Wait for the browser callback or a pasted redirect URL, whichever comes
/// first. Cancelling `cancel` cancels the prompt, which then cancels the
/// sign-in (upstream `waitForAuthorizationResponse`).
async fn wait_for_authorization_response(
	callback: &pi_ai::auth::callback::OAuthCallbackServer,
	state: &str,
	redirect_url: &reqwest::Url,
	prompt: &dyn SignInPrompt,
	cancel: &CancellationToken,
) -> Result<AuthorizationResponse, SignInError> {
	let prompt_cancel = cancel.child_token();
	tokio::select! {
		() = cancel.cancelled() => Err(SignInError::Cancelled),
		outcome = callback.wait() => match outcome {
			pi_ai::auth::callback::CallbackOutcome::Code { code, state: callback_state, iss } => {
				// The shared callback server cannot check the state itself —
				// this sign-in owns the check (upstream
				// `waitForCallback(state, …)` rejects at the server).
				if callback_state.as_deref() != Some(state) {
					Err(SignInError::Invalid(
						"The redirect URL belongs to a different sign-in".to_string(),
					))
				} else {
					Ok(AuthorizationResponse { code, iss })
				}
			}
			pi_ai::auth::callback::CallbackOutcome::ProviderError(message) => {
				Err(SignInError::Invalid(message))
			}
			pi_ai::auth::callback::CallbackOutcome::Cancelled => Err(SignInError::Cancelled),
		},
		pasted = prompt.prompt_for_redirect_url(prompt_cancel) => match pasted {
			Some(input) if !input.trim().is_empty() => {
				response_from_redirect_url(&input, state, redirect_url)
			}
			_ => Err(SignInError::Cancelled),
		},
	}
}

/// Deadline for the whole sign-in, applied around [`sign_in`] by callers
/// (upstream `AbortSignal.timeout`). Kept here so the CLI and the session
/// agree on the shape.
pub fn cancel_after(duration: Duration) -> CancellationToken {
	let token = CancellationToken::new();
	tokio::spawn({
		let token = token.clone();
		async move {
			tokio::time::sleep(duration).await;
			token.cancel();
		}
	});
	token
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn callback_settings_follows_the_upstream_derivation() {
		// Default: ephemeral port, no fixed redirect.
		let settings = callback_settings(&OAuthSettings::default());
		assert_eq!(settings.host, "127.0.0.1");
		assert_eq!(settings.redirect_host, "127.0.0.1");
		assert_eq!(settings.path, "/callback");
		assert_eq!(settings.port, None);
		assert_eq!(settings.fixed_redirect_url, None);

		// callbackPort: fixed redirect at the default host.
		let settings = callback_settings(&OAuthSettings {
			callback_port: Some(8765),
			..Default::default()
		});
		assert_eq!(settings.port, Some(8765));
		assert_eq!(
			settings.fixed_redirect_url.as_deref(),
			Some("http://127.0.0.1:8765/callback")
		);

		// A configured callbackUrl with a port is sent exactly as written.
		let settings = callback_settings(&OAuthSettings {
			redirect_url: Some("http://localhost:9000/auth/callback".to_string()),
			..Default::default()
		});
		assert_eq!(settings.host, "127.0.0.1");
		assert_eq!(settings.redirect_host, "localhost");
		assert_eq!(settings.path, "/auth/callback");
		assert_eq!(settings.port, Some(9000));
		assert_eq!(
			settings.fixed_redirect_url.as_deref(),
			Some("http://localhost:9000/auth/callback")
		);
	}

	#[test]
	fn merge_scopes_deduplicates_in_order() {
		assert_eq!(
			merge_scopes(&[Some("a b"), Some("b c")]),
			Some("a b c".to_string())
		);
		assert_eq!(merge_scopes(&[Some("a"), None]), Some("a".to_string()));
		assert_eq!(merge_scopes(&[None, None]), None);
		assert_eq!(merge_scopes(&[]), None);
		assert_eq!(merge_scopes(&[Some("")]), None);
	}

	fn assert_invalid(result: Result<AuthorizationResponse, SignInError>, message: &str) {
		match result {
			Err(SignInError::Invalid(text)) => assert_eq!(text, message),
			other => panic!("expected Invalid({message:?}), got {other:?}"),
		}
	}

	#[test]
	fn pasted_redirect_validation_matches_upstream_messages() {
		let redirect = reqwest::Url::parse("http://127.0.0.1:5353/callback").unwrap();
		let ok = response_from_redirect_url(
			" http://127.0.0.1:5353/callback?code=c1&state=st&iss=https://as.example.com ",
			"st",
			&redirect,
		)
		.unwrap();
		assert_eq!(ok.code, "c1");
		assert_eq!(ok.iss.as_deref(), Some("https://as.example.com"));

		// Not a URL.
		assert_invalid(
			response_from_redirect_url("nope", "st", &redirect),
			"Expected the full redirect URL from the browser address bar",
		);
		// Different path/origin.
		assert_invalid(
			response_from_redirect_url(
				"http://127.0.0.1:5353/other?code=c&state=st",
				"st",
				&redirect,
			),
			"The redirect URL does not match this sign-in's redirect URI",
		);
		// Error parameter wins, description preferred.
		assert_invalid(
			response_from_redirect_url(
				"http://127.0.0.1:5353/callback?error=access_denied&error_description=Nope&state=st",
				"st",
				&redirect,
			),
			"Nope",
		);
		// State mismatch.
		assert_invalid(
			response_from_redirect_url(
				"http://127.0.0.1:5353/callback?code=c&state=other",
				"st",
				&redirect,
			),
			"The redirect URL belongs to a different sign-in",
		);
		// Missing state counts as mismatch.
		assert_invalid(
			response_from_redirect_url("http://127.0.0.1:5353/callback?code=c", "st", &redirect),
			"The redirect URL belongs to a different sign-in",
		);
		// Missing code.
		assert_invalid(
			response_from_redirect_url("http://127.0.0.1:5353/callback?state=st", "st", &redirect),
			"The redirect URL does not contain an authorization code",
		);
	}

	// -- full-chain mock-server tests ---------------------------------------

	use std::time::Duration;

	use crate::oauth::provider::OAuthStateStore as _;
	use crate::oauth::types::OAuthTokens;
	use crate::test_support::{self, MockRequest, MockResponse};

	/// One authorization server supporting discovery, dynamic registration,
	/// and an authorization-code grant. `token_answer` decides the /token
	/// response per request.
	fn chain_server(
		token_answer: Arc<dyn Fn(&MockRequest) -> MockResponse + Send + Sync>,
	) -> (String, std::thread::JoinHandle<()>) {
		let (base, handle) = test_support::serve(
			// discovery: two protected-resource probes + AS metadata;
			// registration; code exchange.
			5,
			Arc::new(move |request: &MockRequest| {
				match (request.method.as_str(), request.path.as_str()) {
					("GET", "/.well-known/oauth-protected-resource/mcp")
					| ("GET", "/.well-known/oauth-protected-resource") => {
						MockResponse::json("404 Not Found", "none")
					}
					("GET", "/.well-known/oauth-authorization-server") => {
						let host = request
							.raw
							.split("\r\n")
							.find_map(|line| {
								let (name, value) = line.split_once(':')?;
								name.eq_ignore_ascii_case("host")
									.then(|| value.trim().to_string())
							})
							.unwrap_or_default();
						let origin = format!("http://{host}");
						MockResponse::json(
							"200 OK",
							serde_json::json!({
								"issuer": format!("{origin}/"),
								"authorization_endpoint": format!("{origin}/authorize"),
								"token_endpoint": format!("{origin}/token"),
								"registration_endpoint": format!("{origin}/register"),
								"response_types_supported": ["code"],
								"code_challenge_methods_supported": ["S256"],
								"grant_types_supported": ["authorization_code", "refresh_token"]
							})
							.to_string(),
						)
					}
					("POST", "/register") => MockResponse::json(
						"200 OK",
						serde_json::json!({
							"client_id": "client-1",
							"redirect_uris": ["http://127.0.0.1:0/callback"],
							"grant_types": ["authorization_code", "refresh_token"],
							"token_endpoint_auth_method": "none"
						})
						.to_string(),
					),
					("POST", "/token") => token_answer(request),
					other => panic!("unexpected request {other:?}"),
				}
			}),
		);
		(base, handle)
	}

	/// A prompt that records shown URLs and answers the paste prompt from a
	/// slot a test task fills (simulating either the pasted redirect URL or
	/// a browser hitting the callback).
	struct RecordedPrompt {
		urls: Arc<std::sync::Mutex<Vec<String>>>,
		paste: Arc<std::sync::Mutex<Option<Option<String>>>>,
	}

	impl RecordedPrompt {
		fn new() -> Self {
			Self {
				urls: Arc::new(std::sync::Mutex::new(Vec::new())),
				paste: Arc::new(std::sync::Mutex::new(None)),
			}
		}

		fn urls(&self) -> Arc<std::sync::Mutex<Vec<String>>> {
			Arc::clone(&self.urls)
		}

		fn paste_slot(&self) -> Arc<std::sync::Mutex<Option<Option<String>>>> {
			Arc::clone(&self.paste)
		}
	}

	#[async_trait::async_trait]
	impl SignInPrompt for RecordedPrompt {
		fn show_authorization_url(&self, url: &reqwest::Url) {
			self.urls.lock().unwrap().push(url.to_string());
		}

		async fn prompt_for_redirect_url(&self, cancel: CancellationToken) -> Option<String> {
			// A missing paste answer means only the browser callback can
			// finish; poll until the slot is filled or the sign-in is
			// cancelled so a failing test does not hang forever.
			loop {
				if let Some(answer) = self.paste.lock().unwrap().clone() {
					return answer;
				}
				if cancel.is_cancelled() {
					return None;
				}
				tokio::time::sleep(Duration::from_millis(10)).await;
			}
		}
	}

	/// Wait for the shown authorization URL, parse its redirect URI and
	/// state, and return them.
	async fn await_authorization_url(
		urls: Arc<std::sync::Mutex<Vec<String>>>,
	) -> (reqwest::Url, String, reqwest::Url) {
		let deadline = std::time::Instant::now() + Duration::from_secs(5);
		let url = loop {
			if let Some(url) = urls.lock().unwrap().first() {
				break url.clone();
			}
			assert!(
				std::time::Instant::now() < deadline,
				"authorization URL never shown"
			);
			tokio::time::sleep(Duration::from_millis(10)).await;
		};
		let auth = reqwest::Url::parse(&url).unwrap();
		let state = auth
			.query_pairs()
			.find(|(key, _)| key == "state")
			.map(|(_, value)| value.into_owned())
			.unwrap();
		let redirect = reqwest::Url::parse(
			&auth
				.query_pairs()
				.find(|(key, _)| key == "redirect_uri")
				.map(|(_, value)| value.into_owned())
				.unwrap(),
		)
		.unwrap();
		(auth, state, redirect)
	}

	/// application/x-www-form-urlencoded encode for a query value.
	fn urlencoding_form(value: &str) -> String {
		let mut out = String::new();
		for byte in value.as_bytes() {
			match byte {
				b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
					out.push(*byte as char)
				}
				_ => out.push_str(&format!("%{byte:02X}")),
			}
		}
		out
	}

	fn tokens_answer(request: &MockRequest) -> MockResponse {
		assert!(request.raw.contains("grant_type=authorization_code"));
		MockResponse::json(
			"200 OK",
			serde_json::json!({
				"access_token": "at-1",
				"token_type": "Bearer",
				"expires_in": 3600,
				"scope": "mcp read",
				"refresh_token": "rt-1"
			})
			.to_string(),
		)
	}

	#[tokio::test]
	async fn paste_fallback_completes_the_full_flow() {
		let (base, server) = chain_server(Arc::new(tokens_answer));
		let store = Arc::new(super::super::provider::MemoryStateStore::new());
		let prompt = RecordedPrompt::new();
		// Simulate the user pasting the browser's redirect URL.
		let paste_base = base.clone();
		let paste = tokio::spawn({
			let urls = prompt.urls();
			let slot = prompt.paste_slot();
			async move {
				let (_, state, redirect) = await_authorization_url(urls).await;
				let mut pasted = redirect.clone();
				let iss = urlencoding_form(&format!("{paste_base}/"));
				pasted.set_query(Some(&format!("code=cb-1&state={state}&iss={iss}")));
				*slot.lock().unwrap() = Some(Some(pasted.to_string()));
			}
		});
		let options = SignInOptions {
			server_url: format!("{base}/mcp"),
			store: store.clone(),
			settings: OAuthSettings::default(),
			challenge: None,
			cancel: CancellationToken::new(),
		};
		sign_in(options, &prompt).await.unwrap();
		paste.await.unwrap();
		server.join().unwrap();

		// The shown URL carried the fresh state, and the exchange stored the
		// tokens under the server's key.
		assert_eq!(
			prompt.urls.lock().unwrap().len(),
			1,
			"one authorization URL shown"
		);
		let state: Option<OAuthState> = store.load().await.unwrap();
		let state = state.unwrap();
		assert_eq!(state.tokens.as_ref().unwrap().access_token, "at-1");
		assert_eq!(
			state.tokens.as_ref().unwrap().scope.as_deref(),
			Some("mcp read")
		);
	}

	#[tokio::test]
	async fn browser_callback_completes_the_full_flow() {
		let (base, server) = chain_server(Arc::new(tokens_answer));
		let store = Arc::new(super::super::provider::MemoryStateStore::new());
		let prompt = RecordedPrompt::new();
		// Act like the browser: hit the callback server directly.
		let browser_base = base.clone();
		let browser = tokio::spawn({
			let urls = prompt.urls();
			async move {
				let (_, state, redirect) = await_authorization_url(urls).await;
				let base = browser_base;
				let host = redirect.host_str().unwrap().to_string();
				let port = redirect.port().unwrap();
				let path = redirect.path().to_string();
				let iss = urlencoding_form(&format!("{base}/"));
				reqwest::get(format!(
					"http://{host}:{port}{path}?code=cb-1&state={state}&iss={iss}"
				))
				.await
				.unwrap();
			}
		});
		let options = SignInOptions {
			server_url: format!("{base}/mcp"),
			store: store.clone(),
			settings: OAuthSettings::default(),
			challenge: None,
			cancel: CancellationToken::new(),
		};
		sign_in(options, &prompt).await.unwrap();
		browser.await.unwrap();
		server.join().unwrap();
		let state: Option<OAuthState> = store.load().await.unwrap();
		assert_eq!(state.unwrap().tokens.unwrap().access_token, "at-1");
	}

	#[tokio::test]
	async fn step_up_challenge_merges_granted_and_challenged_scope() {
		let (base, server) = chain_server(Arc::new(tokens_answer));
		let store = Arc::new(super::super::provider::MemoryStateStore::new());
		// Tokens with a granted scope; the challenge asks for more.
		store
			.save(OAuthState {
				server_url: String::new(),
				client_information: None,
				tokens: Some(OAuthTokens {
					access_token: "stale".to_string(),
					token_type: "Bearer".to_string(),
					expires_in: None,
					scope: Some("read".to_string()),
					refresh_token: None,
					id_token: None,
					extra: Default::default(),
				}),
				tokens_expire_at: None,
				code_verifier: None,
				oauth_state: None,
				discovery: None,
			})
			.await
			.unwrap();
		let prompt = RecordedPrompt::new();
		let paste = tokio::spawn({
			let urls = prompt.urls();
			let slot = prompt.paste_slot();
			async move {
				let (_, state, redirect) = await_authorization_url(urls).await;
				let mut pasted = redirect;
				pasted.set_query(Some(&format!("code=cb-1&state={state}")));
				*slot.lock().unwrap() = Some(Some(pasted.to_string()));
			}
		});
		let options = SignInOptions {
			server_url: format!("{base}/mcp"),
			store: store.clone(),
			settings: OAuthSettings::default(),
			challenge: Some(Challenge {
				resource_metadata_url: None,
				scope: Some("write".to_string()),
				error: Some("insufficient_scope".to_string()),
				error_description: None,
			}),
			cancel: CancellationToken::new(),
		};
		sign_in(options, &prompt).await.unwrap();
		paste.await.unwrap();
		server.join().unwrap();
		let auth = {
			let urls = prompt.urls.lock().unwrap();
			reqwest::Url::parse(&urls[0]).unwrap()
		};
		let scope = auth
			.query_pairs()
			.find(|(key, _)| key == "scope")
			.map(|(_, value)| value.into_owned())
			.unwrap_or_default();
		assert_eq!(
			scope, "read write",
			"step-up merges granted and challenged scopes"
		);
	}

	#[tokio::test]
	async fn cancellation_yields_cancelled() {
		// Zero connections: any request the flow made would hang the mock
		// and fail the join.
		let (base, server) =
			test_support::serve(0, Arc::new(|_request| MockResponse::json("500 Err", "")));
		let store = Arc::new(super::super::provider::MemoryStateStore::new());
		let prompt = RecordedPrompt::new();
		let cancel = CancellationToken::new();
		let options = SignInOptions {
			server_url: format!("{base}/mcp"),
			store,
			settings: OAuthSettings::default(),
			challenge: None,
			cancel: cancel.clone(),
		};
		cancel.cancel();
		let error = sign_in(options, &prompt).await.unwrap_err();
		assert!(matches!(error, SignInError::Cancelled), "got {error:?}");
		server.join().unwrap();
	}
}
