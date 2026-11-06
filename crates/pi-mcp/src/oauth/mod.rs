//! OAuth for streamable-HTTP MCP servers — port of `pi/packages/mcp/src/oauth/*`
//! and `pi/packages/mcp/src/auth-provider.ts`.
//!
//! Layout mirrors upstream one file per concern: [`types`] (metadata/token
//! wire shapes), [`discovery`] (401 challenge parsing and well-known
//! metadata), [`flow`] (authorization-code + PKCE, dynamic client
//! registration, token refresh), [`provider`] (state stores and the concrete
//! provider/token-provider the transport consumes), [`errors`].
//!
//! Declared differences from the TS original:
//! - **Interactive browser sign-in is deferred.** Upstream opens the
//!   authorization URL with the platform handler and completes the code
//!   exchange against a loopback callback server
//!   (`oauth/callback.ts`, coding-agent `signInMcpServer`). pi-rs keeps the
//!   redirect hook and the whole code-exchange path (an authorization code
//!   handed in through [`flow::FlowOptions::authorization_code`] completes a
//!   started flow), but no UI drives it yet. The supported path today is a
//!   pre-seeded stored token (or refresh token) plus the refresh flow, and a
//!   refresh that is impossible fails with
//!   [`errors::Error::AuthorizationRequired`], like upstream's
//!   `McpOAuthAuthorizationRequiredError`.
//! - The Client ID Metadata Document hook exists in the flow, but pi-rs
//!   ships no built-in document (upstream serves one from pi.dev through the
//!   coding-agent layer).
//! - Upstream `adaptOAuthProvider` (the storage-free demo adapter) is not
//!   ported; [`provider::StoredTokenProvider`] is the port of the
//!   coding-agent `createMcpAuthProvider` that production actually uses.

pub mod discovery;
pub mod errors;
pub mod flow;
pub mod provider;
pub mod types;

pub use errors::Error as OAuthError;
pub use flow::{FlowOptions, FlowResult, authorize};
pub use provider::{
	FileStateStore, McpOAuthProvider, MemoryStateStore, OAuthSettings, StoredTokenProvider,
};
pub use types::{
	AuthorizationServerMetadata, Challenge, ClientInformation, ClientMetadataDocument,
	DiscoveryState, InvalidateKind, OAuthState, OAuthTokens, ProtectedResourceMetadata,
};
