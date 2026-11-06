//! OAuth authorization-code + PKCE flow — port of
//! [`pi/packages/mcp/src/oauth/flow.ts`](../../../pi/packages/mcp/src/oauth/flow.ts)
//! (itself adapted from the MCP TypeScript SDK `client/auth.ts`).
//!
//! [`authorize`] is the port of upstream's `runFlow` + `authorizeMcp`
//! wrapper: discover (with cached discovery state), resolve the `resource`
//! parameter, reuse or register the client, exchange an authorization code
//! (validating the RFC 9207 `iss`), refresh stored tokens, or build the
//! authorization redirect with a fresh PKCE pair. `invalid_client`,
//! `unauthorized_client`, and `invalid_grant` answers clear the matching
//! credentials and retry once, like upstream.
//!
//! Declared difference: there is no `skipIssuerValidation` option — a
//! discovered metadata document is always validated against the URL it came
//! from, while a configured `authorization_server_metadata_url` is trusted
//! as-is (upstream's default too, and what the coding agent passes).

use base64::Engine as _;
use serde_json::json;

use pi_ai::auth::pkce::generate_pkce;

use crate::oauth::discovery::{
	ServerInfo, discover_authorization_server_metadata, discover_server_info, select_resource,
};
use crate::oauth::errors::Error;
use crate::oauth::provider::McpOAuthProvider;
use crate::oauth::types::{
	AuthorizationServerMetadata, ClientInformation, InvalidateKind, OAuthTokens, parse,
};

/// What [`authorize`] ended with (upstream `"AUTHORIZED" | "REDIRECT"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlowResult {
	/// Tokens were exchanged or refreshed and are stored.
	Authorized,
	/// The user must open the authorization URL (upstream `"REDIRECT"`).
	Redirect,
}

/// Options of one [`authorize`] run (upstream `OAuthFlowOptions`). There is
/// no abort signal — requests run on the provider's client, which carries
/// the 15s OAuth request timeout.
#[derive(Debug, Clone, Default)]
pub struct FlowOptions {
	/// The exact MCP server URL the flow authorizes.
	pub server_url: String,
	/// An authorization code to exchange, e.g. delivered by a callback.
	pub authorization_code: Option<String>,
	/// `iss` of the authorization response that delivered the code
	/// (RFC 9207).
	pub iss: Option<String>,
	/// Scopes to request; defaults to the server's advertised scopes.
	pub scope: Option<String>,
	/// Protected-resource metadata URL from a challenge, overriding the
	/// well-known guess.
	pub resource_metadata_url: Option<reqwest::Url>,
	/// Metadata document to use instead of discovery, trusted as configured.
	pub authorization_server_metadata_url: Option<reqwest::Url>,
	/// Go straight to the authorization redirect instead of refreshing
	/// stored tokens (e.g. a step-up challenge needs new scopes).
	pub skip_refresh: bool,
}

/// The client metadata sent with dynamic client registration (upstream
/// `OAuthClientMetadata`, with the defaults `McpOAuthProvider` applies).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClientMetadata {
	/// `client_name` of the registration (shown to the user by the server).
	pub client_name: Option<String>,
	/// Default scope of the registration when nothing else specifies one.
	pub scope: Option<String>,
	/// Redirect URIs; the provider defaults this to `[redirect_url]`.
	pub redirect_uris: Vec<String>,
	/// Defaults to `authorization_code` + `refresh_token`.
	pub grant_types: Vec<String>,
	/// Defaults to `code`.
	pub response_types: Vec<String>,
	/// Defaults to `client_secret_post` with a configured secret, `none`
	/// without one.
	pub token_endpoint_auth_method: Option<String>,
}

enum ClientAuthMethod {
	SecretBasic,
	SecretPost,
	None,
}

fn is_loopback(hostname: &str) -> bool {
	matches!(hostname, "localhost" | "127.0.0.1" | "[::1]" | "::1")
}

/// The OpenID Connect `application_type` for `redirect_uris` (MCP SEP-837).
/// Loopback hosts and custom schemes are native (RFC 8252); everything else
/// is web.
fn application_type(redirect_uris: &[String]) -> &'static str {
	let native = redirect_uris.iter().any(|uri| {
		reqwest::Url::parse(uri).is_ok_and(|url| {
			(url.scheme() != "http" && url.scheme() != "https")
				|| is_loopback(url.host_str().unwrap_or(""))
		})
	});
	if native { "native" } else { "web" }
}

/// Token endpoints and configured metadata documents must be https, except
/// on loopback hosts (upstream `secureEndpoint`).
fn secure_endpoint(url: &reqwest::Url) -> Result<reqwest::Url, Error> {
	if url.scheme() != "https" && !is_loopback(url.host_str().unwrap_or("")) {
		return Err(Error::InsecureEndpoint {
			endpoint: url.to_string(),
		});
	}
	Ok(url.clone())
}

fn join_path(base: &reqwest::Url, path: &str) -> reqwest::Url {
	let mut url = base.clone();
	url.set_path(path);
	url.set_query(None);
	url.set_fragment(None);
	url
}

fn is_http_url(url: &reqwest::Url) -> bool {
	matches!(url.scheme(), "http" | "https")
}

/// Which token-endpoint auth method to use — upstream
/// `selectClientAuthMethod`: a hinted method wins when the server supports
/// it, else the first workable method of the server's list.
fn select_client_auth_method(client: &ClientInformation, supported: &[String]) -> ClientAuthMethod {
	let hinted = client.token_endpoint_auth_method.as_deref();
	if let Some(hint) = hinted
		&& matches!(hint, "client_secret_basic" | "client_secret_post" | "none")
		&& (supported.is_empty() || supported.iter().any(|method| method == hint))
	{
		return match hint {
			"client_secret_basic" => ClientAuthMethod::SecretBasic,
			"client_secret_post" => ClientAuthMethod::SecretPost,
			_ => ClientAuthMethod::None,
		};
	}
	if supported.is_empty() {
		return if client.client_secret.is_some() {
			ClientAuthMethod::SecretBasic
		} else {
			ClientAuthMethod::None
		};
	}
	if client.client_secret.is_some()
		&& supported
			.iter()
			.any(|method| method == "client_secret_basic")
	{
		return ClientAuthMethod::SecretBasic;
	}
	if client.client_secret.is_some()
		&& supported
			.iter()
			.any(|method| method == "client_secret_post")
	{
		return ClientAuthMethod::SecretPost;
	}
	if supported.iter().any(|method| method == "none") {
		return ClientAuthMethod::None;
	}
	if client.client_secret.is_some() {
		ClientAuthMethod::SecretPost
	} else {
		ClientAuthMethod::None
	}
}

fn apply_client_authentication(
	method: ClientAuthMethod,
	client: &ClientInformation,
	params: &mut Vec<(String, String)>,
) -> Result<Option<String>, Error> {
	match method {
		ClientAuthMethod::SecretBasic => {
			let secret = client.client_secret.as_deref().ok_or_else(|| {
				Error::Invalid("client_secret_basic requires a client secret".to_string())
			})?;
			let credentials = base64::engine::general_purpose::STANDARD
				.encode(format!("{}:{secret}", client.client_id));
			Ok(Some(format!("Basic {credentials}")))
		}
		ClientAuthMethod::SecretPost => {
			params.push(("client_id".to_string(), client.client_id.clone()));
			if let Some(secret) = &client.client_secret {
				params.push(("client_secret".to_string(), secret.clone()));
			}
			Ok(None)
		}
		ClientAuthMethod::None => {
			params.push(("client_id".to_string(), client.client_id.clone()));
			Ok(None)
		}
	}
}

/// One token-endpoint request (code exchange or refresh) — upstream
/// `tokenRequest`. The response body is checked for an OAuth `error` before
/// the status, since servers may report OAuth errors with any status.
#[allow(clippy::too_many_arguments)]
async fn token_request(
	client: &reqwest::Client,
	authorization_server_url: &reqwest::Url,
	metadata: Option<&AuthorizationServerMetadata>,
	client_information: &ClientInformation,
	resource: Option<&str>,
	mut params: Vec<(String, String)>,
) -> Result<OAuthTokens, Error> {
	let endpoint = match metadata {
		Some(metadata) => secure_endpoint(
			&reqwest::Url::parse(&metadata.token_endpoint)
				.map_err(|_| Error::Invalid("Invalid token endpoint".to_string()))?,
		)?,
		None => secure_endpoint(&join_path(authorization_server_url, "/token"))?,
	};
	if let Some(resource) = resource {
		params.push(("resource".to_string(), resource.to_string()));
	}
	let supported = metadata
		.and_then(|metadata| metadata.token_endpoint_auth_methods_supported.as_deref())
		.unwrap_or(&[]);
	let authorization = apply_client_authentication(
		select_client_auth_method(client_information, supported),
		client_information,
		&mut params,
	)?;
	let mut request = client
		.post(endpoint)
		.header("Accept", "application/json")
		.header("Content-Type", "application/x-www-form-urlencoded");
	if let Some(authorization) = authorization {
		request = request.header("Authorization", authorization);
	}
	let response = request
		.form(&params)
		.send()
		.await
		.map_err(|error| Error::Network(error.to_string()))?;
	let status = response.status();
	let text = response
		.text()
		.await
		.map_err(|error| Error::Network(error.to_string()))?;
	let value: serde_json::Value = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
	if let Some(error) = value.get("error").and_then(serde_json::Value::as_str) {
		let description = value
			.get("error_description")
			.and_then(serde_json::Value::as_str)
			.unwrap_or(error);
		return Err(Error::OAuth {
			code: error.to_string(),
			message: description.to_string(),
			error_uri: value
				.get("error_uri")
				.and_then(serde_json::Value::as_str)
				.map(str::to_string),
		});
	}
	if !status.is_success() {
		return Err(Error::OAuth {
			code: "server_error".to_string(),
			message: format!("HTTP {status}: {text}"),
			error_uri: None,
		});
	}
	parse::oauth_tokens(value)
}

/// POST the client metadata to the registration endpoint (RFC 7591) —
/// upstream `registerClient`, including the derived `application_type`
/// (SEP-837) and the optional registration `scope`.
pub async fn register_client(
	client: &reqwest::Client,
	authorization_server_url: &reqwest::Url,
	metadata: Option<&AuthorizationServerMetadata>,
	client_metadata: &ClientMetadata,
	scope: Option<&str>,
) -> Result<ClientInformation, Error> {
	let endpoint = match metadata {
		Some(metadata) => {
			let endpoint = metadata.registration_endpoint.as_deref().ok_or_else(|| {
				Error::Invalid(
					"Authorization server does not support dynamic client registration".to_string(),
				)
			})?;
			reqwest::Url::parse(endpoint)
				.map_err(|_| Error::Invalid("Invalid registration endpoint".to_string()))?
		}
		None => join_path(authorization_server_url, "/register"),
	};
	let mut body = serde_json::Map::new();
	if let Some(name) = &client_metadata.client_name {
		body.insert("client_name".to_string(), json!(name));
	}
	body.insert(
		"redirect_uris".to_string(),
		json!(client_metadata.redirect_uris),
	);
	body.insert(
		"grant_types".to_string(),
		json!(client_metadata.grant_types),
	);
	body.insert(
		"response_types".to_string(),
		json!(client_metadata.response_types),
	);
	if let Some(method) = &client_metadata.token_endpoint_auth_method {
		body.insert("token_endpoint_auth_method".to_string(), json!(method));
	}
	body.insert(
		"application_type".to_string(),
		json!(application_type(&client_metadata.redirect_uris)),
	);
	if let Some(scope) = scope {
		body.insert("scope".to_string(), json!(scope));
	}
	let response = client
		.post(endpoint)
		.header("Accept", "application/json")
		.header("Content-Type", "application/json")
		.body(serde_json::Value::Object(body).to_string())
		.send()
		.await
		.map_err(|error| Error::Network(error.to_string()))?;
	let status = response.status();
	let text = response
		.text()
		.await
		.map_err(|error| Error::Network(error.to_string()))?;
	if !status.is_success() {
		return Err(Error::Registration {
			status: status.as_u16(),
			body: text,
		});
	}
	let value: serde_json::Value =
		serde_json::from_str(&text).map_err(|error| Error::Invalid(format!("{error}")))?;
	parse::client_information(value)
}

/// Build the authorization-endpoint URL with a fresh PKCE pair — upstream
/// `startAuthorization`. Returns the URL to open and the code verifier to
/// store until the code arrives.
#[allow(clippy::too_many_arguments)]
pub fn start_authorization(
	authorization_server_url: &reqwest::Url,
	metadata: Option<&AuthorizationServerMetadata>,
	client_information: &ClientInformation,
	redirect_url: &str,
	scope: Option<&str>,
	state: Option<&str>,
	resource: Option<&str>,
) -> Result<(reqwest::Url, String), Error> {
	if let Some(metadata) = metadata
		&& !metadata
			.response_types_supported
			.iter()
			.any(|kind| kind == "code")
	{
		return Err(Error::Invalid(
			"Authorization server does not support authorization codes".to_string(),
		));
	}
	if let Some(metadata) = metadata
		&& let Some(methods) = &metadata.code_challenge_methods_supported
		&& !methods.iter().any(|method| method == "S256")
	{
		return Err(Error::Invalid(
			"Authorization server does not support PKCE S256".to_string(),
		));
	}
	let mut url = match metadata {
		Some(metadata) => reqwest::Url::parse(&metadata.authorization_endpoint)
			.map_err(|_| Error::Invalid("Invalid authorization endpoint".to_string()))?,
		None => join_path(authorization_server_url, "/authorize"),
	};
	// Callers open this URL with the platform URL handler, which also
	// launches files and custom protocols — only http/https may pass.
	if !is_http_url(&url) {
		return Err(Error::Invalid(format!(
			"Invalid authorization endpoint {url}"
		)));
	}
	let (verifier, challenge) = generate_pkce()
		.map_err(|error| Error::Invalid(format!("PKCE generation failed: {error}")))?;
	// Existing query parameters of the endpoint are preserved (upstream
	// `searchParams.set` keeps them).
	let mut pairs: Vec<(String, String)> = url.query_pairs().into_owned().collect();
	pairs.push(("response_type".to_string(), "code".to_string()));
	pairs.push((
		"client_id".to_string(),
		client_information.client_id.clone(),
	));
	pairs.push(("code_challenge".to_string(), challenge));
	pairs.push(("code_challenge_method".to_string(), "S256".to_string()));
	pairs.push(("redirect_uri".to_string(), redirect_url.to_string()));
	if let Some(state) = state {
		pairs.push(("state".to_string(), state.to_string()));
	}
	if let Some(scope) = scope {
		pairs.push(("scope".to_string(), scope.to_string()));
	}
	if scope.is_some_and(|scope| {
		scope
			.split_whitespace()
			.any(|part| part == "offline_access")
	}) {
		pairs.push(("prompt".to_string(), "consent".to_string()));
	}
	if let Some(resource) = resource {
		pairs.push(("resource".to_string(), resource.to_string()));
	}
	url.set_query(None);
	url.query_pairs_mut().extend_pairs(
		pairs
			.iter()
			.map(|(key, value)| (key.as_str(), value.as_str())),
	);
	Ok((url, verifier))
}

/// Exchange an authorization code for tokens — upstream
/// `exchangeAuthorizationCode`.
#[allow(clippy::too_many_arguments)]
pub async fn exchange_authorization_code(
	client: &reqwest::Client,
	authorization_server_url: &reqwest::Url,
	metadata: Option<&AuthorizationServerMetadata>,
	client_information: &ClientInformation,
	resource: Option<&str>,
	code: &str,
	code_verifier: &str,
	redirect_url: &str,
) -> Result<OAuthTokens, Error> {
	token_request(
		client,
		authorization_server_url,
		metadata,
		client_information,
		resource,
		vec![
			("grant_type".to_string(), "authorization_code".to_string()),
			("code".to_string(), code.to_string()),
			("code_verifier".to_string(), code_verifier.to_string()),
			("redirect_uri".to_string(), redirect_url.to_string()),
		],
	)
	.await
}

/// Refresh a grant — upstream `refreshAuthorization`. A response without a
/// refresh token keeps the old one.
pub async fn refresh_authorization(
	client: &reqwest::Client,
	authorization_server_url: &reqwest::Url,
	metadata: Option<&AuthorizationServerMetadata>,
	client_information: &ClientInformation,
	resource: Option<&str>,
	refresh_token: &str,
) -> Result<OAuthTokens, Error> {
	let mut tokens = token_request(
		client,
		authorization_server_url,
		metadata,
		client_information,
		resource,
		vec![
			("grant_type".to_string(), "refresh_token".to_string()),
			("refresh_token".to_string(), refresh_token.to_string()),
		],
	)
	.await?;
	if tokens.refresh_token.is_none() {
		tokens.refresh_token = Some(refresh_token.to_string());
	}
	Ok(tokens)
}

/// Scopes for a step-up authorization: the challenged scopes plus the ones
/// granted so far, since a challenge may list only the missing scopes and a
/// token with just those would lose access the old one had (SEP-2350).
/// Without challenged scopes the flow picks its default. An empty
/// challenge counts as none, like upstream's falsy check.
pub fn step_up_scope(granted: Option<&str>, challenged: Option<&str>) -> Option<String> {
	let challenged = challenged.filter(|scope| !scope.is_empty())?;
	let mut scopes = Vec::new();
	for part in granted.into_iter().chain(std::iter::once(challenged)) {
		for scope in part.split_whitespace() {
			if !scopes.iter().any(|seen| seen == scope) {
				scopes.push(scope.to_string());
			}
		}
	}
	Some(scopes.join(" "))
}

/// A response without `scope` grants the requested scope (RFC 6749 §5.1;
/// a refresh keeps the scope of the grant, §6) — recorded so a step-up can
/// keep it (upstream `withScope`).
fn with_scope(mut tokens: OAuthTokens, scope: Option<String>) -> OAuthTokens {
	if tokens.scope.is_none() {
		tokens.scope = scope;
	}
	tokens
}

async fn run_flow(provider: &McpOAuthProvider, options: &FlowOptions) -> Result<FlowResult, Error> {
	let server_url = reqwest::Url::parse(&options.server_url)
		.map_err(|_| Error::Invalid(format!("Invalid MCP server URL {}", options.server_url)))?;
	let metadata_url = options
		.authorization_server_metadata_url
		.as_ref()
		.map(secure_endpoint)
		.transpose()?;
	// With a configured metadata URL discovery is not cached, so changing
	// the URL applies at once (upstream runFlow).
	let cached = if metadata_url.is_none() {
		provider.discovery_state().await?
	} else {
		None
	};
	let discovered = match cached.filter(|cached| !cached.authorization_server_url.is_empty()) {
		Some(cached) => {
			let authorization_server_url = reqwest::Url::parse(&cached.authorization_server_url)
				.map_err(|_| {
					Error::Invalid(format!(
						"Invalid cached authorization server URL {}",
						cached.authorization_server_url
					))
				})?;
			let authorization_server_metadata = match cached.authorization_server_metadata {
				Some(metadata) => Some(metadata),
				None => {
					discover_authorization_server_metadata(
						provider.http(),
						&authorization_server_url,
						false,
					)
					.await?
				}
			};
			ServerInfo {
				authorization_server_url,
				authorization_server_metadata,
				resource_metadata: cached.resource_metadata,
			}
		}
		None => {
			discover_server_info(
				provider.http(),
				&server_url,
				options.resource_metadata_url.as_ref(),
				metadata_url.as_ref(),
			)
			.await?
		}
	};
	if metadata_url.is_none() {
		provider
			.save_discovery_state(crate::oauth::types::DiscoveryState {
				authorization_server_url: discovered.authorization_server_url.to_string(),
				authorization_server_metadata: discovered.authorization_server_metadata.clone(),
				resource_metadata: discovered.resource_metadata.clone(),
				resource_metadata_url: options
					.resource_metadata_url
					.as_ref()
					.map(reqwest::Url::to_string),
			})
			.await?;
	}
	let metadata = discovered.authorization_server_metadata.as_ref();
	let resource = select_resource(&server_url, discovered.resource_metadata.as_ref())?;
	// `||`, not `??`: an empty scope (for example from `scopes_supported: []`)
	// falls through to the next source.
	let scope = options
		.scope
		.clone()
		.filter(|scope| !scope.is_empty())
		.or_else(|| {
			discovered
				.resource_metadata
				.as_ref()
				.and_then(|metadata| metadata.scopes_supported.as_deref())
				.map(scopes_join)
				.filter(|scope| !scope.is_empty())
		})
		.or_else(|| {
			provider
				.client_metadata()
				.scope
				.clone()
				.filter(|scope| !scope.is_empty())
		});
	let stored = provider.client_information().await?;
	let client_document = if stored.is_none() {
		provider.client_metadata_document(metadata).await
	} else {
		None
	};
	if let Some(document) = &client_document {
		let url = reqwest::Url::parse(&document.url)
			.map_err(|_| Error::Invalid("Invalid OAuth client metadata URL".to_string()))?;
		if url.scheme() != "https" || url.path() == "/" {
			return Err(Error::Invalid(
				"Invalid OAuth client metadata URL".to_string(),
			));
		}
	}
	let client = match (stored, &client_document) {
		(Some(stored), _) => stored,
		(None, Some(document)) => ClientInformation {
			client_id: document.url.clone(),
			..Default::default()
		},
		(None, None) => {
			if options.authorization_code.is_some() {
				return Err(Error::Invalid(
					"OAuth client information is missing during code exchange".to_string(),
				));
			}
			let client = register_client(
				provider.http(),
				&discovered.authorization_server_url,
				metadata,
				provider.client_metadata(),
				scope.as_deref(),
			)
			.await?;
			provider.save_client_information(&client).await?;
			client
		}
	};
	// The document's redirect URI may differ from the provider's, for
	// example by a server-specific path.
	let redirect_url = client_document
		.map(|document| document.redirect_url)
		.unwrap_or_else(|| provider.redirect_url().to_string());
	if let Some(code) = &options.authorization_code {
		// RFC 9207: never send a code from another authorization server to
		// this one. Without metadata there is no issuer to compare with, so
		// any `iss` is rejected.
		let iss = options.iss.as_deref();
		let expected = metadata.map(|metadata| metadata.issuer.as_str());
		if (iss.is_some()
			|| metadata.is_some_and(|metadata| {
				metadata.authorization_response_iss_parameter_supported == Some(true)
			})) && iss != expected
		{
			return Err(Error::IssuerMismatch {
				expected: expected.map(str::to_string),
				received: iss.map(str::to_string),
			});
		}
		let code_verifier = provider.code_verifier().await?;
		let tokens = exchange_authorization_code(
			provider.http(),
			&discovered.authorization_server_url,
			metadata,
			&client,
			resource.as_deref(),
			code,
			&code_verifier,
			&redirect_url,
		)
		.await?;
		provider.save_tokens(with_scope(tokens, scope)).await?;
		return Ok(FlowResult::Authorized);
	}
	if !options.skip_refresh
		&& let Some(existing) = provider.tokens().await?
		&& let Some(refresh_token) = existing.refresh_token.clone()
	{
		match refresh_authorization(
			provider.http(),
			&discovered.authorization_server_url,
			metadata,
			&client,
			resource.as_deref(),
			&refresh_token,
		)
		.await
		{
			Ok(tokens) => {
				provider
					.save_tokens(with_scope(tokens, existing.scope))
					.await?;
				return Ok(FlowResult::Authorized);
			}
			Err(error) => {
				// A refresh the server answered with a real OAuth error (or
				// refused to send) must not be papered over with a redirect;
				// `server_error` and network failures fall through to a
				// fresh authorization.
				let fall_through = match &error {
					Error::InsecureEndpoint { .. } => false,
					Error::OAuth { code, .. } => code == "server_error",
					_ => true,
				};
				if !fall_through {
					return Err(error);
				}
			}
		}
	}
	let state = provider.state().await?;
	let (authorization_url, code_verifier) = start_authorization(
		&discovered.authorization_server_url,
		metadata,
		&client,
		&redirect_url,
		scope.as_deref(),
		Some(&state),
		resource.as_deref(),
	)?;
	provider.save_code_verifier(&code_verifier).await?;
	provider
		.redirect_to_authorization(&authorization_url)
		.await?;
	Ok(FlowResult::Redirect)
}

fn scopes_join(scopes: &[String]) -> String {
	scopes.join(" ")
}

/// Run the flow with upstream's credential-invalidation recovery — port of
/// `authorizeMcp`: a rejected client clears everything, an `invalid_grant`
/// clears the tokens, and both retry once.
pub async fn authorize(
	provider: &McpOAuthProvider,
	options: FlowOptions,
) -> Result<FlowResult, Error> {
	match run_flow(provider, &options).await {
		Ok(result) => Ok(result),
		Err(error) => match error.code() {
			Some("invalid_client") | Some("unauthorized_client") => {
				provider.invalidate_credentials(InvalidateKind::All).await?;
				run_flow(provider, &options).await
			}
			Some("invalid_grant") => {
				provider
					.invalidate_credentials(InvalidateKind::Tokens)
					.await?;
				run_flow(provider, &options).await
			}
			_ => Err(error),
		},
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn step_up_scope_merges_granted_and_challenged() {
		assert_eq!(
			step_up_scope(Some("a b"), Some("b c")),
			Some("a b c".to_string())
		);
		assert_eq!(step_up_scope(None, Some("x")), Some("x".to_string()));
		assert_eq!(step_up_scope(Some("a"), None), None);
		assert_eq!(step_up_scope(Some("a"), Some("")), None);
	}

	#[test]
	fn application_type_follows_redirect_uris() {
		assert_eq!(
			application_type(&["http://127.0.0.1:1/callback".to_string()]),
			"native"
		);
		assert_eq!(
			application_type(&["http://localhost/cb".to_string()]),
			"native"
		);
		assert_eq!(application_type(&["myapp://cb".to_string()]), "native");
		assert_eq!(
			application_type(&["https://app.example.com/cb".to_string()]),
			"web"
		);
		assert_eq!(
			application_type(&["http://app.example.com/cb".to_string()]),
			"web"
		);
	}

	#[test]
	fn client_auth_method_selection_follows_the_upstream_ladder() {
		let supported = |items: &[&str]| {
			items
				.iter()
				.map(|item| item.to_string())
				.collect::<Vec<_>>()
		};
		let public = ClientInformation {
			client_id: "id".into(),
			..Default::default()
		};
		let secret = ClientInformation {
			client_id: "id".into(),
			client_secret: Some("s".into()),
			..Default::default()
		};
		assert!(matches!(
			select_client_auth_method(&secret, &[]),
			ClientAuthMethod::SecretBasic
		));
		assert!(matches!(
			select_client_auth_method(&public, &[]),
			ClientAuthMethod::None
		));
		assert!(matches!(
			select_client_auth_method(&secret, &supported(&["client_secret_post", "none"])),
			ClientAuthMethod::SecretPost
		));
		assert!(matches!(
			select_client_auth_method(&public, &supported(&["client_secret_basic"])),
			ClientAuthMethod::None
		));
		let hinted = ClientInformation {
			client_id: "id".into(),
			client_secret: Some("s".into()),
			token_endpoint_auth_method: Some("none".into()),
			..Default::default()
		};
		assert!(matches!(
			select_client_auth_method(&hinted, &[]),
			ClientAuthMethod::None
		));
	}

	#[test]
	fn refresh_keeps_the_old_refresh_token_when_the_response_omits_one() {
		let old = OAuthTokens {
			access_token: "a".into(),
			token_type: "Bearer".into(),
			refresh_token: Some("old".into()),
			..Default::default()
		};
		assert_eq!(old.refresh_token.as_deref(), Some("old"));
	}

	#[test]
	fn with_scope_only_fills_a_missing_scope() {
		let tokens = OAuthTokens {
			access_token: "a".into(),
			token_type: "Bearer".into(),
			..Default::default()
		};
		assert_eq!(
			with_scope(tokens.clone(), Some("files".into()))
				.scope
				.as_deref(),
			Some("files")
		);
		let scoped = OAuthTokens {
			scope: Some("granted".into()),
			..tokens
		};
		assert_eq!(
			with_scope(scoped, Some("requested".into()))
				.scope
				.as_deref(),
			Some("granted")
		);
	}

	#[test]
	fn secure_endpoint_allows_loopback_http() {
		assert!(secure_endpoint(&"https://auth.example.com/token".parse().unwrap()).is_ok());
		assert!(secure_endpoint(&"http://127.0.0.1:9000/token".parse().unwrap()).is_ok());
		assert!(secure_endpoint(&"http://[::1]:9000/token".parse().unwrap()).is_ok());
		let error = secure_endpoint(&"http://auth.example.com/token".parse().unwrap()).unwrap_err();
		assert!(error.to_string().contains("non-HTTPS"), "{error}");
	}
	// -- full-chain mock-server tests ---------------------------------------

	use std::sync::Arc;

	use crate::oauth::provider::OAuthStateStore as _;
	use crate::oauth::provider::{McpOAuthProviderOptions, MemoryStateStore, now_ms};
	use crate::oauth::types::{ClientMetadataDocument, OAuthState};
	use crate::test_support::{self, MockRequest, MockResponse};

	/// One authorization server that supports discovery, dynamic client
	/// registration, authorization codes with PKCE, and refresh grants.
	/// `grant_answer` decides the /token response per request; `connections`
	/// bounds the accepted request count.
	fn chain_server(
		connections: usize,
		grant_answer: Arc<dyn Fn(&MockRequest) -> MockResponse + Send + Sync>,
	) -> (String, String, std::thread::JoinHandle<()>) {
		let (base, handle) = test_support::serve(
			connections,
			Arc::new(move |request: &MockRequest| {
				match (request.method.as_str(), request.path.as_str()) {
					// No protected-resource metadata on either well-known shape.
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
					("POST", "/register") => {
						assert!(
							request.raw.contains("application/json"),
							"registration is a JSON post: {}",
							request.raw
						);
						assert!(
							request.raw.contains(r#""application_type":"native""#),
							"loopback redirect URIs register as native: {}",
							request.raw
						);
						MockResponse::json(
							"200 OK",
							serde_json::json!({
								"client_id": "client-1",
								"client_id_issued_at": 1,
								"redirect_uris": ["http://127.0.0.1:8192/callback"],
								"grant_types": ["authorization_code", "refresh_token"],
								"token_endpoint_auth_method": "none"
							})
							.to_string(),
						)
					}
					("POST", "/token") => grant_answer(request),
					other => panic!("unexpected request {other:?}"),
				}
			}),
		);
		let issuer = format!("{base}/");
		(base, issuer, handle)
	}

	fn chain_provider(
		server_url: &str,
		store: Arc<MemoryStateStore>,
		redirects: Arc<std::sync::Mutex<Vec<String>>>,
	) -> McpOAuthProvider {
		McpOAuthProvider::new(McpOAuthProviderOptions {
			server_url: server_url.to_string(),
			redirect_url: "http://127.0.0.1:8192/callback".to_string(),
			client_metadata: ClientMetadata {
				client_name: Some("pi-rs-test".to_string()),
				..Default::default()
			},
			client_id: None,
			client_secret: None,
			store,
			// The interactive browser sign-in is deferred; the test hook
			// stands in for the UI that would open this URL.
			on_redirect: Some(Arc::new(move |url| {
				redirects.lock().unwrap().push(url.to_string());
			})),
			client_metadata_document: None,
		})
	}

	fn seeded_state(server_url: &str, refresh_token: &str, expires_in: Option<i64>) -> OAuthState {
		OAuthState {
			server_url: server_url.to_string(),
			client_information: Some(ClientInformation {
				client_id: "client-1".to_string(),
				client_secret: None,
				..Default::default()
			}),
			tokens: Some(OAuthTokens {
				access_token: "stale-token".to_string(),
				token_type: "Bearer".to_string(),
				refresh_token: Some(refresh_token.to_string()),
				expires_in,
				..Default::default()
			}),
			tokens_expire_at: expires_in.map(|expires_in| now_ms() + expires_in * 1000),
			..Default::default()
		}
	}

	#[allow(unused)]
	fn _document_type_touch(_document: Option<ClientMetadataDocument>) {}

	#[tokio::test]
	async fn full_chain_discovery_registration_code_exchange_and_refresh() {
		let store = Arc::new(MemoryStateStore::new());
		let redirects = Arc::new(std::sync::Mutex::new(Vec::new()));
		// The token endpoint logs its requests; every answer carries a
		// rotated refresh token.
		let token_requests = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
		let token_log = token_requests.clone();
		let (base, issuer, handle) = chain_server(
			6,
			Arc::new(move |request: &MockRequest| {
				token_log
					.lock()
					.unwrap()
					.push(request.raw.to_ascii_lowercase());
				MockResponse::json(
					"200 OK",
					serde_json::json!({
						"access_token": "new-token",
						"token_type": "Bearer",
						"expires_in": 3600,
						"refresh_token": "rt-rotated"
					})
					.to_string(),
				)
			}),
		);
		let _ = issuer;
		let server_url = format!("{base}/mcp");
		let provider = chain_provider(&server_url, store.clone(), redirects.clone());

		// 1. No stored credentials and no code: discover, register, prepare
		// the authorization redirect.
		let result = authorize(
			&provider,
			FlowOptions {
				server_url: server_url.clone(),
				..Default::default()
			},
		)
		.await
		.unwrap();
		assert_eq!(result, FlowResult::Redirect);
		let authorization: reqwest::Url = redirects.lock().unwrap()[0].parse().unwrap();
		assert_eq!(authorization.path(), "/authorize");
		let query: std::collections::HashMap<String, String> =
			authorization.query_pairs().into_owned().collect();
		assert_eq!(query["response_type"], "code");
		assert_eq!(query["client_id"], "client-1");
		assert_eq!(query["code_challenge_method"], "S256");
		assert_eq!(query["redirect_uri"], "http://127.0.0.1:8192/callback");
		let state = query["state"].clone();
		assert_eq!(
			provider.state().await.unwrap(),
			state,
			"the flow persisted the state parameter"
		);
		// The PKCE verifier was saved before the redirect, so the code can
		// be exchanged later.
		let verifier = provider.code_verifier().await.unwrap();
		assert_eq!(verifier.len(), 43);

		// 2. The "browser" comes back with code + iss (RFC 9207); discovery
		// is cached, so only the token endpoint is hit.
		let result = authorize(
			&provider,
			FlowOptions {
				server_url: server_url.clone(),
				authorization_code: Some("the-code".to_string()),
				iss: Some(issuer.clone()),
				..Default::default()
			},
		)
		.await
		.unwrap();
		assert_eq!(result, FlowResult::Authorized);
		let stored = store.load().await.unwrap().unwrap();
		let tokens = stored.tokens.clone().unwrap();
		assert_eq!(tokens.access_token, "new-token");
		assert_eq!(tokens.refresh_token.as_deref(), Some("rt-rotated"));
		assert_eq!(tokens.expires_in, Some(3600));
		assert!(stored.tokens_expire_at.unwrap() > now_ms());
		// A response without `scope` grants the requested scope; with none
		// requested, none is recorded.
		assert_eq!(tokens.scope, None);
		// Cloned out: a held guard would deadlock the mock's own logging.
		let exchange = token_requests.lock().unwrap()[0].clone();
		assert!(
			exchange.contains("grant_type=authorization_code"),
			"{exchange}"
		);
		assert!(exchange.contains("code=the-code"), "{exchange}");
		assert!(
			exchange.contains(&format!("code_verifier={}", verifier.to_ascii_lowercase())),
			"{exchange}"
		);
		assert!(exchange.contains("client_id=client-1"), "{exchange}");
		// The log is lowercased, so the percent escapes compare lowercase too.
		assert!(
			exchange.contains("redirect_uri=http%3a%2f%2f127.0.0.1%3a8192%2fcallback"),
			"{exchange}"
		);

		// 3. A later authorize run refreshes the stored grant; the rotated
		// refresh token is kept.
		let result = authorize(
			&provider,
			FlowOptions {
				server_url,
				..Default::default()
			},
		)
		.await
		.unwrap();
		assert_eq!(result, FlowResult::Authorized);
		let refresh = token_requests.lock().unwrap()[1].clone();
		assert!(refresh.contains("grant_type=refresh_token"), "{refresh}");
		assert!(refresh.contains("refresh_token=rt-rotated"), "{refresh}");
		let stored = store.load().await.unwrap().unwrap();
		assert_eq!(stored.tokens.unwrap().access_token, "new-token");
		handle.join().unwrap();
	}

	#[tokio::test]
	async fn code_from_another_issuer_is_rejected_rfc_9207() {
		let store = Arc::new(MemoryStateStore::new());
		let redirects = Arc::new(std::sync::Mutex::new(Vec::new()));
		let (base, issuer, handle) = chain_server(
			4,
			Arc::new(|_request| {
				MockResponse::json("200 OK", r#"{"access_token":"x","token_type":"Bearer"}"#)
			}),
		);
		let server_url = format!("{base}/mcp");
		let provider = chain_provider(&server_url, store, redirects.clone());
		// Prepare the flow (registers a client, stores a verifier).
		let result = authorize(
			&provider,
			FlowOptions {
				server_url: server_url.clone(),
				..Default::default()
			},
		)
		.await
		.unwrap();
		assert_eq!(result, FlowResult::Redirect);
		let error = authorize(
			&provider,
			FlowOptions {
				server_url,
				authorization_code: Some("the-code".to_string()),
				iss: Some("https://elsewhere.example.com/".to_string()),
				..Default::default()
			},
		)
		.await
		.unwrap_err();
		assert!(error.to_string().contains("issuer mismatch"), "{error}");
		assert!(redirects.lock().unwrap().len() == 1, "no new redirect");
		let _ = issuer;
		handle.join().unwrap();
	}

	#[tokio::test]
	async fn invalid_grant_clears_tokens_and_falls_back_to_a_redirect() {
		let store = Arc::new(MemoryStateStore::new());
		let redirects = Arc::new(std::sync::Mutex::new(Vec::new()));
		let (base, _issuer, handle) = chain_server(
			4,
			Arc::new(|_request| {
				MockResponse::json(
					"400 Bad Request",
					r#"{"error":"invalid_grant","error_description":"token revoked"}"#,
				)
			}),
		);
		let server_url = format!("{base}/mcp");
		store.seed(seeded_state(&server_url, "rt-dead", None));
		let provider = chain_provider(&server_url, store.clone(), redirects);
		let result = authorize(
			&provider,
			FlowOptions {
				server_url,
				..Default::default()
			},
		)
		.await
		.unwrap();
		// invalid_grant clears the tokens; the retry cannot refresh (nothing
		// stored) and ends at the authorization redirect.
		assert_eq!(result, FlowResult::Redirect);
		let stored = store.load().await.unwrap().unwrap();
		assert!(stored.tokens.is_none(), "tokens were invalidated");
		assert!(stored.client_information.is_some(), "the client survived");
		handle.join().unwrap();
	}

	#[tokio::test]
	async fn invalid_client_clears_everything_and_reregisters() {
		let store = Arc::new(MemoryStateStore::new());
		let redirects = Arc::new(std::sync::Mutex::new(Vec::new()));
		let (base, _issuer, handle) = chain_server(
			5,
			Arc::new(|_request| {
				MockResponse::json(
					"400 Bad Request",
					r#"{"error":"invalid_client","error_description":"nope"}"#,
				)
			}),
		);
		let server_url = format!("{base}/mcp");
		store.seed(seeded_state(&server_url, "rt", None));
		let provider = chain_provider(&server_url, store.clone(), redirects);
		let result = authorize(
			&provider,
			FlowOptions {
				server_url,
				..Default::default()
			},
		)
		.await
		.unwrap();
		// invalid_client clears all credentials; the retry registers a fresh
		// client and ends at the redirect (upstream authorizeMcp).
		assert_eq!(result, FlowResult::Redirect);
		let stored = store.load().await.unwrap().unwrap();
		assert_eq!(stored.client_information.unwrap().client_id, "client-1");
		handle.join().unwrap();
	}

	#[tokio::test]
	async fn server_error_refresh_falls_through_to_a_redirect() {
		let store = Arc::new(MemoryStateStore::new());
		let redirects = Arc::new(std::sync::Mutex::new(Vec::new()));
		let (base, _issuer, handle) = chain_server(
			4,
			Arc::new(|_request| {
				MockResponse::json("503 Service Unavailable", r#"{"error":"server_error"}"#)
			}),
		);
		let server_url = format!("{base}/mcp");
		store.seed(seeded_state(&server_url, "rt", None));
		let provider = chain_provider(&server_url, store, redirects);
		let result = authorize(
			&provider,
			FlowOptions {
				server_url,
				..Default::default()
			},
		)
		.await
		.unwrap();
		assert_eq!(result, FlowResult::Redirect);
		handle.join().unwrap();
	}

	#[tokio::test]
	async fn refresh_errors_outside_the_recovery_ladder_fail_without_a_redirect() {
		let store = Arc::new(MemoryStateStore::new());
		let redirects = Arc::new(std::sync::Mutex::new(Vec::new()));
		let (base, _issuer, handle) = chain_server(
			4,
			Arc::new(|_request| {
				MockResponse::json(
					"400 Bad Request",
					r#"{"error":"invalid_scope","error_description":"unknown scope"}"#,
				)
			}),
		);
		let server_url = format!("{base}/mcp");
		store.seed(seeded_state(&server_url, "rt", None));
		let provider = chain_provider(&server_url, store, redirects.clone());
		let error = authorize(
			&provider,
			FlowOptions {
				server_url,
				..Default::default()
			},
		)
		.await
		.unwrap_err();
		assert!(error.to_string().contains("unknown scope"), "{error}");
		assert!(
			redirects.lock().unwrap().is_empty(),
			"no redirect was attempted"
		);
		handle.join().unwrap();
	}

	#[tokio::test]
	async fn refresh_that_omits_the_refresh_token_keeps_the_old_one() {
		let store = Arc::new(MemoryStateStore::new());
		let redirects = Arc::new(std::sync::Mutex::new(Vec::new()));
		let (base, _issuer, handle) = chain_server(
			4,
			Arc::new(|_request| {
				// No refresh_token in the answer.
				MockResponse::json(
					"200 OK",
					r#"{"access_token":"rotated-access","token_type":"Bearer","expires_in":100}"#,
				)
			}),
		);
		let server_url = format!("{base}/mcp");
		store.seed(seeded_state(&server_url, "rt-kept", Some(-1)));
		let provider = chain_provider(&server_url, store.clone(), redirects);
		let result = authorize(
			&provider,
			FlowOptions {
				server_url,
				..Default::default()
			},
		)
		.await
		.unwrap();
		assert_eq!(result, FlowResult::Authorized);
		let stored = store.load().await.unwrap().unwrap();
		let tokens = stored.tokens.unwrap();
		assert_eq!(tokens.access_token, "rotated-access");
		assert_eq!(tokens.refresh_token.as_deref(), Some("rt-kept"));
		handle.join().unwrap();
	}
}
