//! OAuth wire types — port of `pi/packages/mcp/src/oauth/types.ts`
//! (itself adapted from the MCP TypeScript SDK).
//!
//! Two wire formats meet here, exactly like upstream: the RFC metadata and
//! token documents use **snake_case** field names (RFC 8414, RFC 7591,
//! RFC 6749), while the persisted per-server state (`OAuthState`,
//! `DiscoveryState`) uses **camelCase** keys, matching the JSON
//! `mcp-auth.json` shape upstream's `McpOAuthProvider`/`McpOAuthState`
//! serialize. Unknown fields are kept through `extra` maps so stored
//! discovery state round-trips like upstream's spread objects.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::errors::Error;

/// RFC 8414 protected-resource metadata (`WWW-Authenticate` discovery).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProtectedResourceMetadata {
	/// The resource identifier this metadata describes (an https URL).
	pub resource: String,
	/// Authorization servers that can issue tokens for the resource.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub authorization_servers: Option<Vec<String>>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub scopes_supported: Option<Vec<String>>,
	/// Unrecognized members, preserved like upstream's object spread.
	#[serde(flatten, skip_serializing_if = "BTreeMap::is_empty")]
	pub extra: BTreeMap<String, Value>,
}

/// RFC 8414 / OpenID Connect authorization-server metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthorizationServerMetadata {
	/// The issuer identifier; validated against the discovery URL.
	pub issuer: String,
	/// The authorization endpoint opened with the redirect.
	pub authorization_endpoint: String,
	/// The token endpoint used for code exchange and refresh.
	pub token_endpoint: String,
	/// RFC 7591 dynamic client registration endpoint, when offered.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub registration_endpoint: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub scopes_supported: Option<Vec<String>>,
	/// Must contain `code` for the authorization-code flow to run.
	pub response_types_supported: Vec<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub grant_types_supported: Option<Vec<String>>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub token_endpoint_auth_methods_supported: Option<Vec<String>>,
	/// Must contain `S256` when present — pi always uses PKCE S256.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub code_challenge_methods_supported: Option<Vec<String>>,
	/// Whether the server accepts Client ID Metadata Documents.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub client_id_metadata_document_supported: Option<bool>,
	/// Whether authorization responses carry an `iss` parameter (RFC 9207).
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub authorization_response_iss_parameter_supported: Option<bool>,
	#[serde(flatten, skip_serializing_if = "BTreeMap::is_empty")]
	pub extra: BTreeMap<String, Value>,
}

/// RFC 6749 token response (also the stored token shape).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct OAuthTokens {
	/// The bearer token sent as `Authorization: Bearer …`.
	pub access_token: String,
	/// Token type as the server reported it (e.g. `Bearer`).
	pub token_type: String,
	/// Access-token lifetime in seconds; `None` when the server sends none.
	#[serde(
		default,
		skip_serializing_if = "Option::is_none",
		deserialize_with = "deserialize_expires_in"
	)]
	pub expires_in: Option<i64>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub scope: Option<String>,
	/// Refresh token; kept across a refresh whose response omits one.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub refresh_token: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub id_token: Option<String>,
	#[serde(flatten, skip_serializing_if = "BTreeMap::is_empty")]
	pub extra: BTreeMap<String, Value>,
}

/// `expires_in` accepts a JSON number or a numeric string (upstream
/// `Number(input.expires_in)`), `null`/`""` count as absent, and a
/// non-finite or non-numeric value is `Invalid expires_in`. Declared
/// difference: fractional seconds are rejected instead of truncated.
fn deserialize_expires_in<'de, D>(deserializer: D) -> Result<Option<i64>, D::Error>
where
	D: serde::Deserializer<'de>,
{
	let raw = Option::<Value>::deserialize(deserializer)?;
	match raw {
		None | Some(Value::Null) => Ok(None),
		Some(Value::String(text)) if text.is_empty() => Ok(None),
		Some(Value::Number(number)) => number
			.as_i64()
			.map(Some)
			.ok_or_else(|| serde::de::Error::custom("Invalid expires_in")),
		Some(Value::String(text)) => text
			.parse::<i64>()
			.map(Some)
			.map_err(|_| serde::de::Error::custom("Invalid expires_in")),
		Some(_) => Err(serde::de::Error::custom("Invalid expires_in")),
	}
}

/// OAuth client information — either a minimal RFC 7591 response
/// (`client_id`/`client_secret`) or the full registration response whose
/// metadata members (`redirect_uris`, `client_name`, …) are kept.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ClientInformation {
	/// The client identifier issued by the authorization server.
	pub client_id: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub client_secret: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub client_id_issued_at: Option<i64>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub client_secret_expires_at: Option<i64>,
	/// Redirect URIs of a full registration response; the first one is the
	/// refresh-path redirect fallback.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub redirect_uris: Option<Vec<String>>,
	/// Requested token-endpoint auth method; hints method selection.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub token_endpoint_auth_method: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub grant_types: Option<Vec<String>>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub response_types: Option<Vec<String>>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub application_type: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub client_name: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub scope: Option<String>,
	#[serde(flatten, skip_serializing_if = "BTreeMap::is_empty")]
	pub extra: BTreeMap<String, Value>,
}

/// A Client ID Metadata Document: an https URL used as `client_id`, plus the
/// redirect URI it lists (upstream `OAuthClientMetadataDocument`).
#[derive(Debug, Clone, PartialEq)]
pub struct ClientMetadataDocument {
	/// The metadata document URL, sent as `client_id`.
	pub url: String,
	/// The redirect URI registered in that document.
	pub redirect_url: String,
}

/// Persisted discovery results (upstream `OAuthDiscoveryState`) — camelCase
/// keys, nested RFC documents snake_case.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveryState {
	/// The authorization server discovered for (or configured before) the
	/// MCP server.
	pub authorization_server_url: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub authorization_server_metadata: Option<AuthorizationServerMetadata>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub resource_metadata: Option<ProtectedResourceMetadata>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub resource_metadata_url: Option<String>,
}

/// What a 401/403 `WWW-Authenticate` challenge asked for (upstream
/// `OAuthChallenge`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Challenge {
	/// `resource_metadata` parameter — where to fetch the protected
	/// resource metadata.
	pub resource_metadata_url: Option<reqwest::Url>,
	/// `scope` parameter — the scopes the server asks for.
	pub scope: Option<String>,
	/// `error` parameter, e.g. `insufficient_scope`.
	pub error: Option<String>,
	/// `error_description` parameter.
	pub error_description: Option<String>,
}

/// Per-server OAuth state (upstream `McpOAuthState`) — the value stored
/// under `mcp__<server>|<url>` in `mcp-auth.json`. All fields except the
/// server URL are optional so partially-written historical entries parse
/// instead of failing the whole file (upstream's TS tolerance).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct OAuthState {
	/// The exact MCP server URL this state belongs to; state for another
	/// server is ignored so credentials never leak across servers.
	pub server_url: String,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub client_information: Option<ClientInformation>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub tokens: Option<OAuthTokens>,
	/// When the access token expires, in ms since the epoch, computed from
	/// `expires_in` when the tokens were saved.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub tokens_expire_at: Option<i64>,
	/// Pending PKCE verifier of a started authorization.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub code_verifier: Option<String>,
	/// The `state` parameter of a started authorization.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub oauth_state: Option<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub discovery: Option<DiscoveryState>,
}

/// What [`crate::oauth::provider::McpOAuthProvider::invalidate_credentials`]
/// clears (upstream `"all" | "client" | "tokens" | "verifier" | "discovery"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidateKind {
	/// Clear every stored credential.
	All,
	/// Forget the registered/configured client.
	Client,
	/// Forget tokens (an `invalid_grant` answer).
	Tokens,
	/// Forget the pending PKCE verifier.
	Verifier,
	/// Forget cached discovery state.
	Discovery,
}

/// Structural validation of a metadata/token document — the port of
/// upstream's hand-written `parse*` validators, with their error messages.
pub mod parse {
	use serde_json::Value;

	use super::{
		AuthorizationServerMetadata, ClientInformation, Error, OAuthTokens,
		ProtectedResourceMetadata,
	};

	/// Parse and validate protected-resource metadata: `resource` and every
	/// listed authorization server must be http/https URLs.
	pub fn protected_resource_metadata(value: Value) -> Result<ProtectedResourceMetadata, Error> {
		let metadata: ProtectedResourceMetadata = serde_json::from_value(value)
			.map_err(|_| Error::Invalid("Invalid OAuth protected resource metadata".to_string()))?;
		validate_http_url(
			&metadata.resource,
			"Invalid OAuth protected resource metadata resource",
		)?;
		if let Some(servers) = &metadata.authorization_servers {
			for server in servers {
				validate_http_url(server, "Invalid authorization server URL")?;
			}
		}
		Ok(metadata)
	}

	/// Parse and validate authorization-server metadata: issuer and both
	/// endpoints must be http/https URLs and `response_types_supported` must
	/// be present.
	pub fn authorization_server_metadata(
		value: Value,
	) -> Result<AuthorizationServerMetadata, Error> {
		let metadata: AuthorizationServerMetadata = serde_json::from_value(value)
			.map_err(|_| Error::Invalid("Invalid authorization server metadata".to_string()))?;
		validate_http_url(&metadata.issuer, "Invalid authorization server issuer")?;
		validate_http_url(
			&metadata.authorization_endpoint,
			"Invalid authorization endpoint",
		)?;
		validate_http_url(&metadata.token_endpoint, "Invalid token endpoint")?;
		if let Some(endpoint) = &metadata.registration_endpoint {
			validate_http_url(endpoint, "Invalid registration endpoint")?;
		}
		Ok(metadata)
	}

	/// Parse a token response — the required members are `access_token` and
	/// `token_type` (an absent or non-finite `expires_in` is rejected during
	/// deserialization, like upstream).
	pub fn oauth_tokens(value: Value) -> Result<OAuthTokens, Error> {
		serde_json::from_value(value)
			.map_err(|_| Error::Invalid("Invalid OAuth token response".to_string()))
	}

	/// Parse a registration response — `client_id` required.
	pub fn client_information(value: Value) -> Result<ClientInformation, Error> {
		serde_json::from_value(value)
			.map_err(|_| Error::Invalid("Invalid OAuth client registration response".to_string()))
	}

	fn validate_http_url(text: &str, message: &str) -> Result<(), Error> {
		let url = reqwest::Url::parse(text).map_err(|_| Error::Invalid(message.to_string()))?;
		// Only http/https: the authorization endpoint is opened with the
		// platform URL handler, so `file:` or custom schemes would launch
		// local programs (upstream `safeUrl`).
		if !matches!(url.scheme(), "http" | "https") {
			return Err(Error::Invalid(message.to_string()));
		}
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use serde_json::json;

	#[test]
	fn metadata_wire_keeps_rfc_snake_case_and_unknowns() {
		let raw = json!({
			"issuer": "https://auth.example.com",
			"authorization_endpoint": "https://auth.example.com/authorize",
			"token_endpoint": "https://auth.example.com/token",
			"response_types_supported": ["code"],
			"code_challenge_methods_supported": ["S256"],
			"custom_extension": {"a": 1}
		});
		let metadata = parse::authorization_server_metadata(raw.clone()).unwrap();
		assert_eq!(metadata.issuer, "https://auth.example.com");
		assert_eq!(
			metadata.extra.get("custom_extension"),
			Some(&json!({"a": 1}))
		);
		// Round-trip keeps the unknown member and the snake_case keys.
		let encoded = serde_json::to_value(&metadata).unwrap();
		assert_eq!(encoded, raw);
	}

	#[test]
	fn metadata_rejects_non_http_schemes_and_missing_response_types() {
		let error = parse::authorization_server_metadata(json!({
			"issuer": "file:///etc",
			"authorization_endpoint": "https://a/authorize",
			"token_endpoint": "https://a/token",
			"response_types_supported": ["code"]
		}))
		.unwrap_err();
		assert!(
			error
				.to_string()
				.contains("Invalid authorization server issuer")
		);
		assert!(
			parse::authorization_server_metadata(json!({
				"issuer": "https://a",
				"authorization_endpoint": "https://a/authorize",
				"token_endpoint": "https://a/token"
			}))
			.is_err(),
			"response_types_supported is required"
		);
	}

	#[test]
	fn tokens_accept_number_or_numeric_string_expires_in() {
		let tokens = parse::oauth_tokens(json!({
			"access_token": "at",
			"token_type": "Bearer",
			"expires_in": 3600
		}))
		.unwrap();
		assert_eq!(tokens.expires_in, Some(3600));
		let tokens = parse::oauth_tokens(json!({
			"access_token": "at",
			"token_type": "Bearer",
			"expires_in": "3600"
		}))
		.unwrap();
		assert_eq!(tokens.expires_in, Some(3600));
		// null counts as absent (Number(null) would mark the token expired
		// at once upstream).
		let tokens = parse::oauth_tokens(json!({
			"access_token": "at",
			"token_type": "Bearer",
			"expires_in": null
		}))
		.unwrap();
		assert_eq!(tokens.expires_in, None);
		assert!(
			parse::oauth_tokens(json!({
				"access_token": "at",
				"token_type": "Bearer",
				"expires_in": "abc"
			}))
			.is_err()
		);
		assert!(
			parse::oauth_tokens(json!({"access_token": "at"})).is_err(),
			"token_type required"
		);
	}

	#[test]
	fn state_uses_camel_case_keys_with_snake_case_nested_documents() {
		let state = OAuthState {
			server_url: "https://mcp.example.com/mcp".into(),
			tokens: Some(OAuthTokens {
				access_token: "at".into(),
				token_type: "Bearer".into(),
				expires_in: Some(60),
				refresh_token: Some("rt".into()),
				..Default::default()
			}),
			tokens_expire_at: Some(1_700_000_000_000),
			code_verifier: Some("verifier".into()),
			..Default::default()
		};
		let encoded = serde_json::to_value(&state).unwrap();
		assert_eq!(encoded["serverUrl"], "https://mcp.example.com/mcp");
		assert_eq!(encoded["tokensExpireAt"], 1_700_000_000_000i64);
		assert_eq!(encoded["codeVerifier"], "verifier");
		assert_eq!(encoded["tokens"]["access_token"], "at");
		assert_eq!(encoded["tokens"]["refresh_token"], "rt");
		assert_eq!(encoded["tokens"]["expires_in"], 60);
		// Absent fields stay absent, like upstream's deletions.
		assert!(encoded.get("clientInformation").is_none());
		assert!(encoded.get("oauthState").is_none());
		let decoded: OAuthState = serde_json::from_value(encoded).unwrap();
		assert_eq!(decoded, state);
	}

	#[test]
	fn partial_state_entries_still_parse() {
		let state: OAuthState = serde_json::from_value(json!({"serverUrl": "https://x"})).unwrap();
		assert_eq!(state.server_url, "https://x");
		assert!(state.tokens.is_none());
		// A wholly foreign entry parses to an empty state whose server_url
		// never matches, so it is ignored rather than failing the file.
		let state: OAuthState = serde_json::from_value(json!({})).unwrap();
		assert_eq!(state.server_url, "");
	}
}
