//! Auth types — port of `pi/packages/ai/src/auth/types.ts`.
//!
//! `Credential` is the auth.json shape (`{ type: "api_key" | "oauth", ... }`);
//! `OAuthFlow` is the `OAuthAuth` interface each provider login flow
//! implements. `AuthInteraction` is the `AuthInteraction` callback bundle the
//! app supplies (CLI prints + stdin, TUI dialogs).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

/// Request auth for a single model request (`ModelAuth`). Values that cannot
/// be expressed as api key / headers / base URL are provider config, not auth.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelAuth {
	pub api_key: Option<String>,
	pub headers: BTreeMap<String, String>,
	pub base_url: Option<String>,
}

/// Stored api-key credential (`ApiKeyCredential`). `env` holds
/// provider-scoped environment/config values.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ApiKeyCredential {
	pub key: Option<String>,
	pub env: Option<BTreeMap<String, String>>,
}

/// Stored canonical OAuth credential (`OAuthCredential`). `expires` is the
/// epoch-millisecond JS number; extra provider fields (e.g. codex
/// `accountId`) survive the round trip through the flattened extra map.
#[derive(Debug, Clone, PartialEq)]
pub struct OAuthCredential {
	pub access: String,
	pub refresh: String,
	/// Epoch milliseconds (JS number semantics — always an integer here).
	pub expires: i64,
	/// Additional provider-specific fields, preserved as written.
	pub extra: BTreeMap<String, serde_json::Value>,
}

/// One type-tagged credential per provider — the shape of auth.json
/// (`Credential`). Serde produces exactly the upstream JSON: an internal
/// `type` tag, camelCase-free field names as written, unknown fields
/// preserved on oauth entries via the flattened extra map.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Credential {
	#[serde(rename = "api_key")]
	ApiKey {
		#[serde(default, skip_serializing_if = "Option::is_none")]
		key: Option<String>,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		env: Option<BTreeMap<String, String>>,
	},
	#[serde(rename = "oauth")]
	OAuth {
		access: String,
		refresh: String,
		expires: i64,
		/// Provider-specific extra fields (accountId, ...), kept verbatim.
		#[serde(flatten)]
		extra: BTreeMap<String, serde_json::Value>,
	},
}

impl Credential {
	/// The oauth half of a stored credential, if it is one.
	pub fn as_oauth(&self) -> Option<OAuthCredential> {
		match self {
			Credential::OAuth {
				access,
				refresh,
				expires,
				extra,
			} => Some(OAuthCredential {
				access: access.clone(),
				refresh: refresh.clone(),
				expires: *expires,
				extra: extra.clone(),
			}),
			Credential::ApiKey { .. } => None,
		}
	}

	/// The api-key half of a stored credential, if it is one.
	pub fn as_api_key(&self) -> Option<ApiKeyCredential> {
		match self {
			Credential::ApiKey { key, env } => Some(ApiKeyCredential {
				key: key.clone(),
				env: env.clone(),
			}),
			Credential::OAuth { .. } => None,
		}
	}

	/// Non-secret type label for status UI (`CredentialInfo.type`).
	pub fn type_label(&self) -> &'static str {
		match self {
			Credential::ApiKey { .. } => "api_key",
			Credential::OAuth { .. } => "oauth",
		}
	}
}

impl From<OAuthCredential> for Credential {
	fn from(value: OAuthCredential) -> Self {
		Credential::OAuth {
			access: value.access,
			refresh: value.refresh,
			expires: value.expires,
			extra: value.extra,
		}
	}
}

/// Non-secret credential metadata for account/status enumeration.
#[derive(Debug, Clone, PartialEq)]
pub struct CredentialInfo {
	pub provider_id: String,
	/// `"api_key"` or `"oauth"`.
	pub r#type: String,
}

/// Login prompt shown to the user (`AuthPrompt`). `signal` cancels a pending
/// prompt when an out-of-band event resolves the step.
#[derive(Debug, Clone)]
pub struct AuthPrompt {
	pub kind: AuthPromptKind,
	/// Cancels this specific prompt (manual_code raced against a callback).
	pub signal: CancellationToken,
}

#[derive(Debug, Clone)]
pub enum AuthPromptKind {
	Text {
		message: String,
		placeholder: Option<String>,
	},
	Secret {
		message: String,
		placeholder: Option<String>,
	},
	Select {
		message: String,
		options: Vec<SelectOption>,
	},
	ManualCode {
		message: String,
		placeholder: Option<String>,
	},
}

#[derive(Debug, Clone, PartialEq)]
pub struct SelectOption {
	pub id: String,
	pub label: String,
	pub description: Option<String>,
}

/// Login interaction event (`AuthEvent`).
#[derive(Debug, Clone)]
pub enum AuthEvent {
	Info {
		message: String,
	},
	AuthUrl {
		url: String,
		instructions: Option<String>,
	},
	DeviceCode {
		user_code: String,
		verification_uri: String,
		open_browser_on_confirm: bool,
		copy_code_on_confirm: bool,
		interval_seconds: Option<u64>,
		expires_in_seconds: Option<u64>,
	},
	Progress {
		message: String,
	},
}

/// Login interaction callbacks serving both api-key and OAuth flows
/// (`AuthInteraction`). `prompt` resolves with the entered/selected string
/// (select resolves with the option id) or errors on cancel/abort.
pub trait AuthInteraction: Send + Sync {
	/// Cancellation for the whole login flow.
	fn signal(&self) -> CancellationToken;

	fn prompt(
		&self,
		prompt: AuthPrompt,
	) -> futures::future::BoxFuture<'_, Result<String, AuthError>>;

	fn notify(&self, event: AuthEvent);
}

/// App-supplied context for [`OAuthFlow::login`] (`LoginOptions`).
#[derive(Debug, Clone, Default)]
pub struct LoginOptions {
	/// Stable ID of this app installation (OpenAI agent host ID).
	pub get_device_id: Option<String>,
	/// Name this app introduces itself with (Codex originator). Defaults to
	/// pi's own name.
	pub agent_name: Option<String>,
}

/// Provider-owned OAuth flow (`OAuthAuth`): the `refresh`/`to_auth` split
/// lets the caller own the locked refresh pattern.
pub trait OAuthFlow: Send + Sync {
	/// Display name, e.g. "Anthropic (Claude Pro/Max)".
	fn name(&self) -> &str;

	/// Whether access through this auth method is backed by a subscription.
	fn is_subscription(&self) -> bool {
		false
	}

	/// Interactive login producing a fresh credential. The explicit shared
	/// `'a` lets implementations move `interaction` into their returned
	/// future (the flow awaits prompts across the whole login).
	fn login<'a>(
		&'a self,
		interaction: &'a dyn AuthInteraction,
		options: &'a LoginOptions,
	) -> futures::future::BoxFuture<'a, Result<OAuthCredential, AuthError>>;

	/// Exchange the refresh token (network call; errors on invalid_grant).
	/// Runs under the credential-store lock.
	fn refresh<'a>(
		&'a self,
		credential: &'a OAuthCredential,
	) -> futures::future::BoxFuture<'a, Result<OAuthCredential, AuthError>>;

	/// Side-effect-free derivation of request auth from a valid credential.
	fn to_auth(&self, credential: &OAuthCredential) -> Result<ModelAuth, AuthError>;
}

/// Auth-domain error (`ModelsError` codes "auth"/"oauth" + login failures).
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
	#[error("{0}")]
	Login(String),
	#[error("OAuth refresh failed for {provider}: {message}")]
	Refresh { provider: String, message: String },
	#[error("Credential store read failed for {provider}: {message}")]
	StoreRead { provider: String, message: String },
	#[error("Credential store modify failed for {provider}: {message}")]
	StoreModify { provider: String, message: String },
	#[error("{0}")]
	OAuth(String),
	#[error("{0}")]
	Cancelled(String),
}

pub type AuthResult<T> = std::result::Result<T, AuthError>;
