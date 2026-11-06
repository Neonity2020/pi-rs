//! OAuth errors — port of `pi/packages/mcp/src/oauth/errors.ts`. The five
//! upstream error classes become enum variants; `Display` strings follow the
//! upstream messages (the issuer pair is formatted like upstream's
//! constructor: JSON-quoted, or `none` when absent).

/// OAuth failure modes (upstream `OAuthError`, `OAuthIssuerMismatchError`,
/// `OAuthInsecureEndpointError`, `OAuthRegistrationError`,
/// `McpOAuthAuthorizationRequiredError`, plus the structural-validation
/// errors upstream throws as bare `Error("Invalid …")`).
#[derive(Debug)]
pub enum Error {
	/// Token/registration endpoint reported an OAuth error code
	/// (`error` in the response body; RFC 6749 §5.2).
	OAuth {
		/// Upstream `error.code` — e.g. `invalid_grant`, `server_error`.
		code: String,
		/// `error_description`, else the `error` code itself.
		message: String,
		/// `error_uri` when the server sent one.
		error_uri: Option<String>,
	},
	/// The discovered (or answered) issuer differs from the expected one
	/// (RFC 8414 §4.3 validation, RFC 9207 `iss` checks).
	IssuerMismatch {
		/// `None` when no authorization server metadata was discovered.
		expected: Option<String>,
		/// `None` when an authorization response lacks the promised `iss`.
		received: Option<String>,
	},
	/// Token endpoints and configured metadata documents must be https,
	/// except on loopback hosts.
	InsecureEndpoint {
		/// The rejected endpoint URL.
		endpoint: String,
	},
	/// Dynamic client registration (RFC 7591) failed.
	Registration {
		/// HTTP status the registration endpoint returned.
		status: u16,
		/// Response body excerpt.
		body: String,
	},
	/// A new sign-in is required but no interactive flow is available —
	/// upstream `McpOAuthAuthorizationRequiredError`.
	AuthorizationRequired,
	/// Structural validation / flow-state failures with upstream's bare
	/// `Error("…")` messages (`Invalid …`, "No OAuth PKCE code verifier is
	/// stored", …).
	Invalid(String),
	/// A network-level failure of an OAuth request — upstream lets the
	/// fetch `TypeError` propagate past the "resource metadata may be
	/// missing" swallow in `discoverOAuthServerInfo`.
	Network(String),
}

/// An issuer formatted like upstream's error message: JSON-quoted, or
/// `none` when absent.
struct IssuerDisplay<'a>(Option<&'a String>);

impl std::fmt::Display for IssuerDisplay<'_> {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self.0 {
			None => f.write_str("none"),
			Some(issuer) => {
				write!(
					f,
					"{}",
					serde_json::to_string(issuer).unwrap_or_else(|_| issuer.clone())
				)
			}
		}
	}
}

impl std::fmt::Display for Error {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Error::OAuth { message, .. } => f.write_str(message),
			Error::IssuerMismatch { expected, received } => write!(
				f,
				"OAuth issuer mismatch: expected {}, received {}",
				IssuerDisplay(expected.as_ref()),
				IssuerDisplay(received.as_ref())
			),
			Error::InsecureEndpoint { endpoint } => {
				write!(
					f,
					"Refusing to send OAuth credentials to non-HTTPS endpoint {endpoint}"
				)
			}
			Error::Registration { status, body } => write!(
				f,
				"OAuth dynamic client registration failed with status {status}: {body}"
			),
			Error::AuthorizationRequired => {
				f.write_str("MCP OAuth authorization requires user interaction")
			}
			Error::Invalid(message) => f.write_str(message),
			Error::Network(message) => write!(f, "OAuth request failed: {message}"),
		}
	}
}

impl std::error::Error for Error {}

impl Error {
	/// Upstream `error.code`, for callers that branch on OAuth error codes.
	pub fn code(&self) -> Option<&str> {
		match self {
			Error::OAuth { code, .. } => Some(code),
			_ => None,
		}
	}

	/// Whether the error came from the transport layer rather than the
	/// OAuth response (the upstream `TypeError` test).
	pub fn is_network(&self) -> bool {
		matches!(self, Error::Network(_))
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn messages_follow_the_upstream_wording() {
		let error = Error::OAuth {
			code: "invalid_grant".into(),
			message: "Token expired".into(),
			error_uri: None,
		};
		assert_eq!(error.to_string(), "Token expired");
		assert_eq!(error.code(), Some("invalid_grant"));
		let error = Error::IssuerMismatch {
			expected: Some("https://a".into()),
			received: None,
		};
		assert_eq!(
			error.to_string(),
			r#"OAuth issuer mismatch: expected "https://a", received none"#
		);
		assert_eq!(
			Error::InsecureEndpoint {
				endpoint: "http://a/token".into()
			}
			.to_string(),
			"Refusing to send OAuth credentials to non-HTTPS endpoint http://a/token"
		);
		assert_eq!(
			Error::Registration {
				status: 400,
				body: "no".into()
			}
			.to_string(),
			"OAuth dynamic client registration failed with status 400: no"
		);
		assert_eq!(
			Error::AuthorizationRequired.to_string(),
			"MCP OAuth authorization requires user interaction"
		);
		assert!(Error::Network("boom".into()).is_network());
	}
}
