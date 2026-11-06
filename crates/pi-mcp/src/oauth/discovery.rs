//! OAuth discovery — port of `pi/packages/mcp/src/oauth/discovery.ts`
//! (itself adapted from the MCP TypeScript SDK `client/auth.ts`).
//!
//! Order of operations on a 401: parse the `WWW-Authenticate` bearer
//! challenge, fetch the protected-resource metadata it names (or the
//! well-known fallback), take the first authorization server it lists (or
//! the MCP server origin itself), and fetch that server's metadata from the
//! RFC 8414 / OpenID Connect well-known candidates. 4xx and 502 answers
//! mean "not here" and move to the next candidate; the issuer of a found
//! document is validated against the URL it was discovered from.

use serde_json::Value;

use crate::protocol::types::LATEST_PROTOCOL_VERSION;

use super::errors::Error;
use super::types::{AuthorizationServerMetadata, Challenge, ProtectedResourceMetadata, parse};

/// 4xx and 502 mean "not here", so discovery tries the next candidate URL.
fn is_discovery_miss(status: u16) -> bool {
	(400..500).contains(&status) || status == 502
}

/// Path suffix for `/.well-known/<kind><path>`; empty for the root path.
fn path_suffix(pathname: &str) -> String {
	pathname.strip_suffix('/').unwrap_or(pathname).to_string()
}

/// One `name="value"` / `name=value` parameter of a challenge header, with
/// upstream's boundary rule (`(?:^|[,\s])name=`), case-insensitive name,
/// and empty values counting as absent.
fn challenge_field(header: &str, name: &str) -> Option<String> {
	let lowered = header.to_ascii_lowercase();
	let needle = name.to_ascii_lowercase();
	let bytes = lowered.as_bytes();
	let mut index = 0usize;
	while let Some(offset) = lowered[index..].find(&needle) {
		let start = index + offset;
		let after = start + needle.len();
		let boundary_ok = start == 0 || matches!(bytes[start - 1], b',' | b' ' | b'\t');
		if boundary_ok && bytes.get(after) == Some(&b'=') {
			let rest = &header[after + 1..];
			let value = match rest.strip_prefix('"') {
				Some(quoted) => quoted.split('"').next().unwrap_or(""),
				None => rest.split([',', ' ', '\t']).next().unwrap_or(""),
			};
			// An empty value (`scope=""`) carries no information, so it
			// counts as absent.
			if !value.is_empty() {
				return Some(value.to_string());
			}
		}
		index = after;
	}
	None
}

/// Parse a `WWW-Authenticate` header of a 401 (or 403 step-up) answer —
/// upstream `parseWwwAuthenticate`. Only `Bearer` and `DPoP` challenges
/// carry MCP parameters; anything else yields an empty challenge.
pub fn parse_www_authenticate(header: Option<&str>) -> Challenge {
	let Some(header) = header else {
		return Challenge::default();
	};
	let scheme = header
		.split_whitespace()
		.next()
		.unwrap_or("")
		.to_ascii_lowercase();
	if scheme != "bearer" && scheme != "dpop" {
		return Challenge::default();
	}
	Challenge {
		resource_metadata_url: challenge_field(header, "resource_metadata")
			.and_then(|value| reqwest::Url::parse(&value).ok()),
		scope: challenge_field(header, "scope"),
		error: challenge_field(header, "error"),
		error_description: challenge_field(header, "error_description"),
	}
}

async fn fetch_metadata(
	client: &reqwest::Client,
	url: &reqwest::Url,
) -> Result<reqwest::Response, Error> {
	client
		.get(url.clone())
		.header("Accept", "application/json")
		.header("MCP-Protocol-Version", LATEST_PROTOCOL_VERSION)
		.send()
		.await
		.map_err(|error| Error::Network(error.to_string()))
}

async fn metadata_json(response: reqwest::Response, what: &str) -> Result<Value, Error> {
	let text = response
		.text()
		.await
		.map_err(|error| Error::Network(error.to_string()))?;
	serde_json::from_str(&text)
		.map_err(|error| Error::Invalid(format!("{what} is not valid JSON: {error}")))
}

/// Fetch the protected-resource metadata of an MCP server — upstream
/// `discoverProtectedResourceMetadata`. With `resource_metadata_url` (from
/// a challenge) only that URL is tried; otherwise the path-suffixed
/// well-known URL runs first and a miss falls back to the root one.
pub async fn discover_protected_resource_metadata(
	client: &reqwest::Client,
	server_url: &reqwest::Url,
	resource_metadata_url: Option<&reqwest::Url>,
) -> Result<ProtectedResourceMetadata, Error> {
	let explicit = resource_metadata_url.is_some();
	let mut url = resource_metadata_url
		.cloned()
		.unwrap_or_else(|| well_known_url(server_url, "oauth-protected-resource", true));
	let mut response = fetch_metadata(client, &url).await?;
	if !explicit && server_url.path() != "/" && is_discovery_miss(response.status().as_u16()) {
		// The root fallback drops the MCP server's path (upstream fetches
		// `new URL("/.well-known/…", server.origin)`).
		url = well_known_url(server_url, "oauth-protected-resource", false);
		response = fetch_metadata(client, &url).await?;
	}
	if !response.status().is_success() {
		return Err(Error::Invalid(format!(
			"HTTP {} loading OAuth protected resource metadata",
			response.status()
		)));
	}
	parse::protected_resource_metadata(
		metadata_json(response, "OAuth protected resource metadata").await?,
	)
}

/// `/.well-known/<kind><suffix>` under the server origin; `with_path`
/// appends the MCP server's path suffix (query and fragment dropped, like
/// upstream's `new URL(…, server.origin)`).
fn well_known_url(server_url: &reqwest::Url, kind: &str, with_path: bool) -> reqwest::Url {
	let mut url = server_url.clone();
	let suffix = if with_path {
		path_suffix(server_url.path())
	} else {
		String::new()
	};
	url.set_path(&format!("/.well-known/{kind}{suffix}"));
	url.set_query(None);
	url.set_fragment(None);
	url
}

/// Well-known candidate URLs of an authorization server, in try order —
/// upstream `buildAuthorizationServerDiscoveryUrls`: the RFC 8414 document
/// path-suffixed, then OpenID Connect discovery path-suffixed, then (for a
/// pathful issuer) OpenID discovery under the issuer path itself.
pub fn build_authorization_server_discovery_urls(
	authorization_server_url: &reqwest::Url,
) -> Vec<reqwest::Url> {
	let suffix = path_suffix(authorization_server_url.path());
	let mut urls = vec![
		well_known_url(authorization_server_url, "oauth-authorization-server", true),
		well_known_url(authorization_server_url, "openid-configuration", true),
	];
	if !suffix.is_empty() {
		let mut nested = authorization_server_url.clone();
		nested.set_path(&format!("{suffix}/.well-known/openid-configuration"));
		nested.set_query(None);
		nested.set_fragment(None);
		urls.push(nested);
	}
	urls
}

/// Fetch and validate authorization-server metadata — upstream
/// `discoverAuthorizationServerMetadata`. Every candidate miss moves to the
/// next URL; `None` means no candidate answered with metadata. The
/// discovered issuer must equal the URL metadata came from (compare
/// without a trailing slash: URL parsing adds one to bare origins).
pub async fn discover_authorization_server_metadata(
	client: &reqwest::Client,
	authorization_server_url: &reqwest::Url,
	skip_issuer_validation: bool,
) -> Result<Option<AuthorizationServerMetadata>, Error> {
	for url in build_authorization_server_discovery_urls(authorization_server_url) {
		let response = fetch_metadata(client, &url).await?;
		let status = response.status().as_u16();
		if !response.status().is_success() {
			if is_discovery_miss(status) {
				continue;
			}
			return Err(Error::Invalid(format!(
				"HTTP {status} loading authorization server metadata from {url}"
			)));
		}
		let metadata = parse::authorization_server_metadata(
			metadata_json(response, "authorization server metadata").await?,
		)?;
		if !skip_issuer_validation {
			let expected = authorization_server_url.as_str();
			if trim_trailing_slash(&metadata.issuer) != trim_trailing_slash(expected) {
				return Err(Error::IssuerMismatch {
					expected: Some(expected.to_string()),
					received: Some(metadata.issuer.clone()),
				});
			}
		}
		return Ok(Some(metadata));
	}
	Ok(None)
}

fn trim_trailing_slash(value: &str) -> &str {
	value.strip_suffix('/').unwrap_or(value)
}

/// Everything discovery learned about one MCP server (upstream
/// `OAuthServerInfo`, returned by `discoverOAuthServerInfo`).
#[derive(Debug, Clone)]
pub struct ServerInfo {
	/// The authorization server to run the flow against: the first server
	/// the resource metadata lists, else the MCP server origin itself.
	pub authorization_server_url: reqwest::Url,
	/// Metadata document of that server, when one was discovered.
	pub authorization_server_metadata: Option<AuthorizationServerMetadata>,
	/// Protected-resource metadata, when the server offers it. Discovery
	/// failures other than network errors are tolerated here (upstream
	/// swallows everything but the fetch `TypeError`).
	pub resource_metadata: Option<ProtectedResourceMetadata>,
}

/// Discover the authorization server for an MCP server URL — upstream
/// `discoverOAuthServerInfo`. A configured `authorization_server_metadata_url`
/// replaces discovery and is trusted as-is (its issuer names the server).
pub async fn discover_server_info(
	client: &reqwest::Client,
	server_url: &reqwest::Url,
	resource_metadata_url: Option<&reqwest::Url>,
	authorization_server_metadata_url: Option<&reqwest::Url>,
) -> Result<ServerInfo, Error> {
	let resource_metadata =
		match discover_protected_resource_metadata(client, server_url, resource_metadata_url).await
		{
			Ok(metadata) => Some(metadata),
			// Network failures surface; a server without resource metadata (or
			// with an invalid document) falls back to the origin-as-issuer
			// shape of the spec.
			Err(error) if error.is_network() => return Err(error),
			Err(_) => None,
		};
	if let Some(metadata_url) = authorization_server_metadata_url {
		let response = fetch_metadata(client, metadata_url).await?;
		if !response.status().is_success() {
			return Err(Error::Invalid(format!(
				"HTTP {} loading authorization server metadata from {metadata_url}",
				response.status()
			)));
		}
		let metadata = parse::authorization_server_metadata(
			metadata_json(response, "authorization server metadata").await?,
		)?;
		let authorization_server_url = reqwest::Url::parse(&metadata.issuer)
			.map_err(|_| Error::Invalid("Invalid issuer".to_string()))?;
		return Ok(ServerInfo {
			authorization_server_url,
			authorization_server_metadata: Some(metadata),
			resource_metadata,
		});
	}
	let authorization_server_url = resource_metadata
		.as_ref()
		.and_then(|metadata| metadata.authorization_servers.as_ref())
		.and_then(|servers| servers.first())
		.and_then(|server| reqwest::Url::parse(server).ok())
		.unwrap_or_else(|| {
			let mut origin = server_url.clone();
			origin.set_path("/");
			origin
		});
	let authorization_server_metadata =
		discover_authorization_server_metadata(client, &authorization_server_url, false).await?;
	Ok(ServerInfo {
		authorization_server_url,
		authorization_server_metadata,
		resource_metadata,
	})
}

/// The MCP server URL without its fragment — the `resource` value is
/// compared against this (upstream `resourceUrlFromServerUrl`).
pub fn resource_url_from_server_url(server_url: &reqwest::Url) -> reqwest::Url {
	let mut url = server_url.clone();
	url.set_fragment(None);
	url
}

/// The `resource` parameter for token and authorization requests — upstream
/// `selectResource`: the configured resource must share the MCP server's
/// origin and cover its path, so tokens are not sent to a different
/// resource than the one metadata advertised.
pub fn select_resource(
	server_url: &reqwest::Url,
	metadata: Option<&ProtectedResourceMetadata>,
) -> Result<Option<String>, Error> {
	let Some(metadata) = metadata else {
		return Ok(None);
	};
	let requested = resource_url_from_server_url(server_url);
	let configured = reqwest::Url::parse(&metadata.resource).map_err(|_| {
		Error::Invalid("Invalid OAuth protected resource metadata resource".to_string())
	})?;
	if requested.origin() != configured.origin() {
		return Err(Error::Invalid(format!(
			"Protected resource {} does not match MCP server {}",
			metadata.resource, requested
		)));
	}
	if !path_starts_with(requested.path(), configured.path()) {
		return Err(Error::Invalid(format!(
			"Protected resource {} does not match MCP server {}",
			metadata.resource, requested
		)));
	}
	Ok(Some(metadata.resource.clone()))
}

/// Both paths normalized to carry a trailing slash, then a prefix check.
fn path_starts_with(requested: &str, configured: &str) -> bool {
	let with_slash = |path: &str| {
		if path.ends_with('/') {
			path.to_string()
		} else {
			format!("{path}/")
		}
	};
	with_slash(requested).starts_with(&with_slash(configured))
}

#[cfg(test)]
mod tests {
	use super::*;
	use serde_json::json;

	#[test]
	fn challenge_parameters_parse_with_upstream_rules() {
		let challenge = parse_www_authenticate(Some(
			r#"Bearer realm="x", resource_metadata="https://mcp.example.com/.well-known/oauth-protected-resource", scope="files.read write", error="insufficient_scope""#,
		));
		assert_eq!(
			challenge.resource_metadata_url,
			Some(
				"https://mcp.example.com/.well-known/oauth-protected-resource"
					.parse()
					.unwrap()
			)
		);
		assert_eq!(challenge.scope.as_deref(), Some("files.read write"));
		assert_eq!(challenge.error.as_deref(), Some("insufficient_scope"));
		// Unquoted values and non-bearer schemes.
		let challenge = parse_www_authenticate(Some("Bearer error=invalid_token"));
		assert_eq!(challenge.error.as_deref(), Some("invalid_token"));
		assert_eq!(
			parse_www_authenticate(Some("Basic realm=x")),
			Challenge::default()
		);
		assert_eq!(parse_www_authenticate(None), Challenge::default());
		// Empty values count as absent.
		let challenge = parse_www_authenticate(Some(r#"Bearer scope="""#));
		assert_eq!(challenge.scope, None);
	}

	#[test]
	fn discovery_url_candidates_follow_the_spec_shapes() {
		let issuer: reqwest::Url = "https://auth.example.com/tenant1".parse().unwrap();
		let urls = build_authorization_server_discovery_urls(&issuer);
		assert_eq!(
			urls.iter().map(reqwest::Url::as_str).collect::<Vec<_>>(),
			vec![
				"https://auth.example.com/.well-known/oauth-authorization-server/tenant1",
				"https://auth.example.com/.well-known/openid-configuration/tenant1",
				"https://auth.example.com/tenant1/.well-known/openid-configuration",
			]
		);
		let root: reqwest::Url = "https://auth.example.com/".parse().unwrap();
		let urls = build_authorization_server_discovery_urls(&root);
		assert_eq!(
			urls.iter().map(reqwest::Url::as_str).collect::<Vec<_>>(),
			vec![
				"https://auth.example.com/.well-known/oauth-authorization-server",
				"https://auth.example.com/.well-known/openid-configuration",
			]
		);
	}

	#[test]
	fn well_known_urls_use_the_origin_plus_path() {
		let server: reqwest::Url = "https://mcp.example.com/api/mcp".parse().unwrap();
		assert_eq!(
			well_known_url(&server, "oauth-protected-resource", true).as_str(),
			"https://mcp.example.com/.well-known/oauth-protected-resource/api/mcp"
		);
		assert_eq!(
			well_known_url(&server, "oauth-protected-resource", false).as_str(),
			"https://mcp.example.com/.well-known/oauth-protected-resource"
		);
		let root: reqwest::Url = "http://127.0.0.1:8080/".parse().unwrap();
		assert_eq!(
			well_known_url(&root, "oauth-protected-resource", true).as_str(),
			"http://127.0.0.1:8080/.well-known/oauth-protected-resource"
		);
	}

	#[test]
	fn select_resource_matches_origin_and_path() {
		let server: reqwest::Url = "https://mcp.example.com/mcp".parse().unwrap();
		let metadata = ProtectedResourceMetadata {
			resource: "https://mcp.example.com/".into(),
			authorization_servers: None,
			scopes_supported: None,
			extra: Default::default(),
		};
		assert_eq!(
			select_resource(&server, Some(&metadata)).unwrap(),
			Some("https://mcp.example.com/".to_string())
		);
		let other = ProtectedResourceMetadata {
			resource: "https://other.example.com/".into(),
			..metadata.clone()
		};
		let error = select_resource(&server, Some(&other)).unwrap_err();
		assert!(error.to_string().contains("does not match MCP server"));
		let narrower = ProtectedResourceMetadata {
			resource: "https://mcp.example.com/other/".into(),
			..metadata
		};
		assert!(select_resource(&server, Some(&narrower)).is_err());
		assert_eq!(select_resource(&server, None).unwrap(), None);
		// Fragments do not take part in the comparison.
		let fragment_url: reqwest::Url = "https://mcp.example.com/mcp#frag".parse().unwrap();
		assert_eq!(
			resource_url_from_server_url(&fragment_url).as_str(),
			"https://mcp.example.com/mcp"
		);
	}

	#[test]
	fn issuer_comparison_ignores_one_trailing_slash() {
		assert_eq!(trim_trailing_slash("https://a/"), "https://a");
		assert_eq!(trim_trailing_slash("https://a"), "https://a");
	}

	#[tokio::test]
	async fn protected_resource_metadata_falls_back_to_the_root_well_known() {
		let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
		let address = listener.local_addr().unwrap();
		let body = json!({"resource": format!("http://{address}/mcp"), "authorization_servers": [format!("http://{address}/")]});
		let handle = std::thread::spawn(move || {
			// Path-suffixed well-known misses; the root fallback answers.
			for (expected_path, status) in [
				("/.well-known/oauth-protected-resource/mcp", "404 Not Found"),
				("/.well-known/oauth-protected-resource", "200 OK"),
			] {
				let (mut stream, _) = listener.accept().unwrap();
				let request = crate::test_support::read_request(&mut stream);
				assert!(
					request.starts_with(&format!("GET {expected_path} ")),
					"expected {expected_path}, got: {request}"
				);
				let body_text = body.to_string();
				crate::test_support::respond(
					&mut stream,
					status,
					"Content-Type: application/json\r\n",
					if status == "200 OK" {
						body_text.as_str()
					} else {
						""
					},
				);
			}
		});
		let server: reqwest::Url = format!("http://{address}/mcp").parse().unwrap();
		let metadata = discover_protected_resource_metadata(&reqwest::Client::new(), &server, None)
			.await
			.unwrap();
		assert_eq!(metadata.authorization_servers.unwrap().len(), 1);
		handle.join().unwrap();
	}

	#[tokio::test]
	async fn authorization_server_metadata_validates_the_issuer() {
		let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
		let address = listener.local_addr().unwrap();
		let issuer = format!("http://{address}/");
		let body = json!({
			"issuer": "https://elsewhere.example.com/",
			"authorization_endpoint": "https://elsewhere.example.com/authorize",
			"token_endpoint": "https://elsewhere.example.com/token",
			"response_types_supported": ["code"]
		});
		let handle = std::thread::spawn(move || {
			let (mut stream, _) = listener.accept().unwrap();
			let request = crate::test_support::read_request(&mut stream);
			assert!(request.starts_with("GET /.well-known/oauth-authorization-server "));
			crate::test_support::respond(
				&mut stream,
				"200 OK",
				"Content-Type: application/json\r\n",
				&body.to_string(),
			);
		});
		let issuer_url: reqwest::Url = issuer.parse().unwrap();
		let error =
			discover_authorization_server_metadata(&reqwest::Client::new(), &issuer_url, false)
				.await
				.unwrap_err();
		assert!(
			error.to_string().contains("OAuth issuer mismatch"),
			"{error}"
		);
		handle.join().unwrap();
	}
}
