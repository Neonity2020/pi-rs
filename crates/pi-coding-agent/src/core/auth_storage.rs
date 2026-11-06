//! auth.json credential storage — port of
//! `pi/packages/coding-agent/src/core/auth-storage.ts` wiring plus the
//! request-time auth resolution slice of `ModelRuntime` (`model-runtime.ts`).
//!
//! The file lives at `<agentDir>/auth.json` (`AuthStorage::create` upstream).
//! A stored credential **owns** the provider: ambient sources (models.json
//! `apiKey`, adapter env fallbacks) apply only when nothing is stored —
//! upstream's resolve.ts rule. Resolved auth merges into
//! [`pi_ai::StreamOptions`] as `api_key` / `auth_headers` / a cloned model
//! with `base_url` overridden.
//!
//! Like upstream's `sharedAuthFileReadState`, one process-wide
//! [`AuthContext`] serves every consumer (stream fn, `auth` CLI, `/login`).

use std::path::PathBuf;
use std::sync::Arc;

use pi_ai::auth::{
	AuthStorage, Credential, DEFAULT_OAUTH_MINIMUM_VALIDITY_MS, ModelAuth, OAuthFlow,
	resolve_stored_auth,
};

use crate::config;
use crate::core::model_registry::resolve_config_value;

/// The store path: `<agentDir>/auth.json`.
pub fn auth_json_path() -> PathBuf {
	config::agent_dir().join("auth.json")
}

/// Process-wide auth context — file-backed store plus provider-flow lookup.
pub struct AuthContext {
	store: AuthStorage,
}

impl AuthContext {
	/// Open (not create) the store at `<agentDir>/auth.json`.
	pub fn shared() -> &'static AuthContext {
		static SHARED: std::sync::OnceLock<AuthContext> = std::sync::OnceLock::new();
		SHARED.get_or_init(|| AuthContext {
			store: AuthStorage::new(auth_json_path()),
		})
	}

	/// Store at an explicit path (tests).
	pub fn at(path: PathBuf) -> AuthContext {
		AuthContext {
			store: AuthStorage::new(path),
		}
	}

	pub fn store(&self) -> &AuthStorage {
		&self.store
	}

	/// The OAuth flow registered for `provider`, if any.
	pub fn flow_for(&self, provider: &str) -> Option<Arc<dyn OAuthFlow>> {
		pi_ai::auth::oauth_flow_for(provider)
	}

	/// Read the stored credential with configured api-key values resolved
	/// (upstream `AuthStorage.read` applies `resolveConfigValue` to
	/// `api_key` entries with the credential's `env` overlay).
	pub fn read_credential(&self, provider: &str) -> Option<Credential> {
		match self.store.read(provider) {
			Ok(Some(Credential::ApiKey { key, env })) => {
				let resolved = key.and_then(|key| resolve_config_value(&key, env.as_ref()));
				Some(Credential::ApiKey { key: resolved, env })
			}
			Ok(other) => other,
			// An unreadable auth.json behaves as "nothing stored"; the auth
			// CLI surfaces the real error through its own read.
			Err(_) => None,
		}
	}

	/// Non-secret credential listing (provider + type), unreadable file = empty.
	pub fn list_credentials(&self) -> Vec<(String, String)> {
		self.store
			.list()
			.unwrap_or_default()
			.into_iter()
			.map(|info| (info.provider_id, info.r#type))
			.collect()
	}

	/// Sync resolution attempt for the request path: stored api-key
	/// credentials and non-expiring OAuth tokens resolve without I/O.
	/// `Ok(None)` = nothing usable stored (ambient sources apply); the async
	/// [`AuthContext::resolve_auth`] handles expiring OAuth tokens.
	pub fn try_resolve_sync(&self, provider: &str) -> Result<Option<(ModelAuth, String)>, String> {
		let Some(credential) = self.read_credential(provider) else {
			return Ok(None);
		};
		match credential {
			Credential::ApiKey { key, .. } => match key {
				Some(key) => Ok(Some((
					ModelAuth {
						api_key: Some(key),
						..Default::default()
					},
					"stored credential".to_string(),
				))),
				None => Ok(None),
			},
			Credential::OAuth {
				access, expires, ..
			} => {
				let Some(flow) = self.flow_for(provider) else {
					// A stored credential of a type without a matching
					// handler resolves to nothing (no silent env fallback).
					return Ok(None);
				};
				let now = std::time::SystemTime::now()
					.duration_since(std::time::UNIX_EPOCH)
					.map(|d| d.as_millis() as i64)
					.unwrap_or(0);
				if now + DEFAULT_OAUTH_MINIMUM_VALIDITY_MS >= expires {
					return Err("oauth-refresh-required".to_string());
				}
				let auth = flow
					.to_auth(&pi_ai::auth::OAuthCredential {
						access,
						refresh: String::new(),
						expires,
						extra: Default::default(),
					})
					.map_err(|error| error.to_string())?;
				Ok(Some((auth, "OAuth".to_string())))
			}
		}
	}

	/// Full resolution with the locked refresh for expiring OAuth tokens
	/// (upstream `resolveProviderAuth` — the stored-credential branch).
	pub async fn resolve_auth(
		&self,
		provider: &str,
		min_validity_ms: Option<i64>,
	) -> Result<Option<(ModelAuth, String)>, String> {
		let Some(credential) = self.store.read(provider).map_err(|e| e.to_string())? else {
			return Ok(None);
		};
		match credential {
			Credential::ApiKey { key, env } => {
				let resolved = key.and_then(|key| resolve_config_value(&key, env.as_ref()));
				Ok(resolved.map(|key| {
					(
						ModelAuth {
							api_key: Some(key),
							..Default::default()
						},
						"stored credential".to_string(),
					)
				}))
			}
			Credential::OAuth { .. } => {
				let Some(flow) = self.flow_for(provider) else {
					return Ok(None);
				};
				match resolve_stored_auth(&self.store, provider, flow.as_ref(), min_validity_ms)
					.await
				{
					Ok(Some(auth)) => Ok(Some((auth, "OAuth".to_string()))),
					Ok(None) => Ok(None),
					Err(error) => Err(error.to_string()),
				}
			}
		}
	}

	/// Persist a fresh credential after login (upstream
	/// `modify(provider, async () => credential)`).
	pub async fn save_credential(
		&self,
		provider: &str,
		credential: Credential,
	) -> Result<(), String> {
		let store = &self.store;
		let stored = credential.clone();
		store
			.modify::<_, _, (), std::convert::Infallible>(provider, move |_current| {
				let stored = stored.clone();
				async move { Ok((Some(()), Some(stored))) }
			})
			.await
			.and_then(|inner| inner.map(|_| ()).map_err(|e| e.to_string()))
	}

	/// Remove a credential (logout).
	pub async fn delete_credential(&self, provider: &str) -> Result<(), String> {
		self.store.delete(provider).await
	}
}
