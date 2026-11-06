//! Bearer-token supply for the streamable-HTTP transport — port of
//! `pi/packages/mcp/src/auth-provider.ts`.

use crate::oauth::OAuthError;

/// What the transport hands an auth provider after a rejected request
/// (upstream `UnauthorizedContext`).
pub struct UnauthorizedContext {
	/// The rejected response's HTTP status: 401, or 403 whose challenge
	/// reports `insufficient_scope`.
	pub status: u16,
	/// The raw `WWW-Authenticate` header, when the response carried one.
	pub www_authenticate: Option<String>,
	/// The MCP server URL the rejected request went to.
	pub server_url: String,
	/// Access token the rejected request carried, if any. A different
	/// current token means another request already refreshed it.
	pub token: Option<String>,
}

/// Supplies bearer tokens to an MCP HTTP transport and may refresh them
/// after a 401 response (upstream `AuthProvider`). The transport calls
/// [`AuthProvider::token`] before every request whose answer it waits for
/// and [`AuthProvider::on_unauthorized`] at most once per request.
#[async_trait::async_trait]
pub trait AuthProvider: Send + Sync {
	/// Token for the next request; `None` sends the request unauthenticated.
	async fn token(&self) -> Option<String>;

	/// The server rejected the request; refresh or fail. A failure aborts
	/// the request (upstream propagates the throw into `send`).
	async fn on_unauthorized(&self, context: UnauthorizedContext) -> Result<(), OAuthError>;
}
