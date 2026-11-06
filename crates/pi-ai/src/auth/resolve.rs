//! Stored-credential resolution — port of `pi/packages/ai/src/auth/
//! resolve.ts` (the stored-oauth half; ambient api-key resolution stays with
//! the coding-agent registry, which owns models.json/env fallbacks).
//!
//! A stored credential owns the provider: the refresh runs under the
//! credential-store lock so concurrent callers and processes refresh only
//! once, and the result is persisted before the lock releases.

use std::time::{SystemTime, UNIX_EPOCH};

use super::store::AuthStorage;
use super::types::{AuthError, AuthResult, Credential, ModelAuth, OAuthCredential, OAuthFlow};

pub const DEFAULT_OAUTH_MINIMUM_VALIDITY_MS: i64 = 5 * 60 * 1000;
const DEFAULT_OAUTH_REFRESH_TIMEOUT_MS: u64 = 15_000;

fn now_ms() -> i64 {
	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map(|d| d.as_millis() as i64)
		.unwrap_or(0)
}

fn expires_soon(credential: &OAuthCredential, minimum_validity_ms: i64) -> bool {
	now_ms() + minimum_validity_ms >= credential.expires
}

/// Resolve request auth from the stored credential. `Ok(None)` = nothing
/// stored for this provider (ambient sources apply); a stored credential of
/// a type without a matching handler also resolves `None` — no silent env
/// fallback after a failed refresh (upstream rule).
pub async fn resolve_stored_auth(
	store: &AuthStorage,
	provider_id: &str,
	flow: &dyn OAuthFlow,
	min_validity_ms: Option<i64>,
) -> AuthResult<Option<ModelAuth>> {
	let minimum_validity_ms = DEFAULT_OAUTH_MINIMUM_VALIDITY_MS.max(min_validity_ms.unwrap_or(0));
	let stored = store
		.read(provider_id)
		.map_err(|message| AuthError::StoreRead {
			provider: provider_id.to_string(),
			message,
		})?;
	let Some(stored) = stored else {
		return Ok(None);
	};
	let Some(credential) = stored.as_oauth() else {
		// API-key credentials resolve through the registry's config-value
		// layer, not here.
		return Ok(None);
	};
	let credential = if expires_soon(&credential, minimum_validity_ms) {
		// Optimistic check said expired; the authoritative check runs under
		// the lock.
		let post = refresh_stored_oauth_credential(store, provider_id, flow, |current| {
			expires_soon(current, minimum_validity_ms)
		})
		.await?;
		let Some(post) = post else {
			return Ok(None); // logged out meanwhile
		};
		// Explicit callers (bearer-token export) do require the requested
		// minimum after the refresh.
		if min_validity_ms.is_some_and(|requested| {
			expires_soon(&post, DEFAULT_OAUTH_MINIMUM_VALIDITY_MS.max(requested))
		}) {
			return Err(AuthError::OAuth(format!(
				"OAuth refresh returned a token that expires too soon for {provider_id}"
			)));
		}
		post
	} else {
		credential
	};
	flow.to_auth(&credential).map(Some).map_err(|error| {
		AuthError::OAuth(format!(
			"OAuth auth derivation failed for {provider_id}: {error}"
		))
	})
}

/// Refresh a stored OAuth credential under the credential-store lock and
/// persist the result before the lock is released
/// (`refreshStoredOAuthCredential`). The refresh is bounded by a 15s
/// timeout; once it starts it cannot be cancelled (the provider may already
/// have rotated the refresh token). `Ok(None)` = no oauth credential left
/// (logged out) or another process refreshed.
pub async fn refresh_stored_oauth_credential(
	store: &AuthStorage,
	provider_id: &str,
	flow: &dyn OAuthFlow,
	needs_refresh: impl Fn(&OAuthCredential) -> bool + Send,
) -> AuthResult<Option<OAuthCredential>> {
	let provider = provider_id.to_string();
	let outcome = store
		.modify(provider_id, |current| {
			let provider = provider.clone();
			async move {
				let Some(current) = current else {
					return Ok((None, None)); // logged out meanwhile
				};
				let Some(credential) = current.as_oauth() else {
					return Ok((None, None));
				};
				if !needs_refresh(&credential) {
					// Another process/request refreshed.
					return Ok((None, Some(current)));
				}
				match tokio::time::timeout(
					std::time::Duration::from_millis(DEFAULT_OAUTH_REFRESH_TIMEOUT_MS),
					flow.refresh(&credential),
				)
				.await
				{
					Ok(Ok(next)) => Ok((Some(next.clone()), Some(Credential::from(next)))),
					Ok(Err(error)) => Err(AuthError::Refresh {
						provider,
						message: error.to_string(),
					}),
					Err(_elapsed) => Err(AuthError::Refresh {
						provider,
						message: "OAuth refresh timed out".to_string(),
					}),
				}
			}
		})
		.await
		.map_err(|message| AuthError::StoreModify {
			provider: provider_id.to_string(),
			message,
		})?;
	// Propagate the closure's rejection (upstream ModelsError("oauth", ...)).
	outcome
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::auth::types::{AuthError, AuthInteraction, LoginOptions, OAuthCredential};
	use std::convert::Infallible;
	use std::path::PathBuf;
	use std::sync::atomic::{AtomicU32, Ordering};

	/// Controllable fake flow: `refresh` bumps a counter and returns a token
	/// that is valid for an hour (or fails, per the flag).
	struct FakeFlow {
		refresh_calls: AtomicU32,
		fail_refresh: bool,
	}

	const FAKE_FLOW_STATIC: fn() -> FakeFlow = || FakeFlow {
		refresh_calls: AtomicU32::new(0),
		fail_refresh: false,
	};

	impl OAuthFlow for FakeFlow {
		fn name(&self) -> &str {
			"Fake"
		}

		fn login(
			&self,
			_interaction: &dyn AuthInteraction,
			_options: &LoginOptions,
		) -> futures::future::BoxFuture<'_, Result<OAuthCredential, AuthError>> {
			Box::pin(async { Err(AuthError::Login("unused".into())) })
		}

		fn refresh(
			&self,
			credential: &OAuthCredential,
		) -> futures::future::BoxFuture<'_, Result<OAuthCredential, AuthError>> {
			let previous = credential.refresh.clone();
			Box::pin(async move {
				if self.fail_refresh {
					return Err(AuthError::Login("invalid_grant".to_string()));
				}
				self.refresh_calls.fetch_add(1, Ordering::SeqCst);
				let now = std::time::SystemTime::now()
					.duration_since(std::time::UNIX_EPOCH)
					.map(|d| d.as_millis() as i64)
					.unwrap_or(0);
				Ok(OAuthCredential {
					access: format!("refreshed-{}", self.refresh_calls.load(Ordering::SeqCst)),
					refresh: previous,
					expires: now + 3_600_000,
					extra: Default::default(),
				})
			})
		}

		fn to_auth(&self, credential: &OAuthCredential) -> Result<ModelAuth, AuthError> {
			Ok(ModelAuth {
				api_key: Some(credential.access.clone()),
				..Default::default()
			})
		}
	}

	fn temp_store(name: &str) -> (PathBuf, AuthStorage) {
		let dir =
			std::env::temp_dir().join(format!("pi-auth-resolve-{name}-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		std::fs::create_dir_all(&dir).unwrap();
		let path = dir.join("auth.json");
		(path.clone(), AuthStorage::new(path))
	}

	async fn insert_credential(store: &AuthStorage, provider: &str, credential: Credential) {
		store
			.modify::<_, _, (), Infallible>(provider, |_current| {
				let credential = credential.clone();
				async move { Ok((None, Some(credential))) }
			})
			.await
			.unwrap()
			.unwrap();
	}

	fn now_ms() -> i64 {
		std::time::SystemTime::now()
			.duration_since(std::time::UNIX_EPOCH)
			.map(|d| d.as_millis() as i64)
			.unwrap_or(0)
	}

	fn credential(expires_in_ms: i64) -> Credential {
		Credential::OAuth {
			access: "stale".to_string(),
			refresh: "rt".to_string(),
			expires: now_ms() + expires_in_ms,
			extra: Default::default(),
		}
	}

	#[tokio::test]
	async fn fresh_token_resolves_without_refresh() {
		let (_dir, store) = temp_store("fresh");
		insert_credential(&store, "p", credential(3_600_000)).await;
		let flow = FAKE_FLOW_STATIC();
		let auth = resolve_stored_auth(&store, "p", &flow, None)
			.await
			.unwrap()
			.unwrap();
		assert_eq!(auth.api_key.as_deref(), Some("stale"));
		assert_eq!(flow.refresh_calls.load(Ordering::SeqCst), 0);
		let _ = std::fs::remove_dir_all(_dir);
	}

	#[tokio::test]
	async fn expiring_token_refreshes_once_and_persists() {
		let (_dir, store) = temp_store("expiring");
		insert_credential(&store, "p", credential(60_000)).await; // < 5 min window
		let flow = FAKE_FLOW_STATIC();
		let auth = resolve_stored_auth(&store, "p", &flow, None)
			.await
			.unwrap()
			.unwrap();
		assert_eq!(auth.api_key.as_deref(), Some("refreshed-1"));
		assert_eq!(flow.refresh_calls.load(Ordering::SeqCst), 1);
		// The refreshed token is persisted (and a second resolve does not
		// refresh again — the stored expiry moved an hour out).
		let stored = store.read("p").unwrap().unwrap();
		assert_eq!(stored.as_oauth().unwrap().access, "refreshed-1");
		let auth = resolve_stored_auth(&store, "p", &flow, None)
			.await
			.unwrap()
			.unwrap();
		assert_eq!(auth.api_key.as_deref(), Some("refreshed-1"));
		assert_eq!(flow.refresh_calls.load(Ordering::SeqCst), 1);
		let _ = std::fs::remove_dir_all(_dir);
	}

	#[tokio::test]
	async fn refresh_failure_preserves_the_stored_credential() {
		let (_dir, store) = temp_store("refresh-fail");
		insert_credential(&store, "p", credential(60_000)).await;
		let flow = FakeFlow {
			refresh_calls: AtomicU32::new(0),
			fail_refresh: true,
		};
		let error = resolve_stored_auth(&store, "p", &flow, None)
			.await
			.unwrap_err();
		assert!(error.to_string().contains("invalid_grant"), "{error}");
		// Upstream: the stored credential is preserved for retry.
		assert_eq!(
			store.read("p").unwrap().unwrap().as_oauth().unwrap().access,
			"stale"
		);
		let _ = std::fs::remove_dir_all(_dir);
	}

	#[tokio::test]
	async fn nothing_stored_and_api_key_entries_resolve_none() {
		let (_dir, store) = temp_store("none");
		let flow = FAKE_FLOW_STATIC();
		assert!(
			resolve_stored_auth(&store, "p", &flow, None)
				.await
				.unwrap()
				.is_none()
		);
		insert_credential(
			&store,
			"p",
			Credential::ApiKey {
				key: Some("k".into()),
				env: None,
			},
		)
		.await;
		assert!(
			resolve_stored_auth(&store, "p", &flow, None)
				.await
				.unwrap()
				.is_none(),
			"api-key credentials resolve through the registry layer"
		);
		let _ = std::fs::remove_dir_all(_dir);
	}

	#[tokio::test]
	async fn explicit_min_validity_enforced_after_refresh() {
		let (_dir, store) = temp_store("min-validity");
		// A token 10 minutes out: inside the default 5-minute refresh window?
		// No — 10 min > 5 min, so no refresh; but an explicit 30-minute
		// requirement forces the refresh (upstream bearer-token export).
		insert_credential(&store, "p", credential(10 * 60_000)).await;
		let flow = FAKE_FLOW_STATIC();
		let auth = resolve_stored_auth(&store, "p", &flow, Some(30 * 60_000))
			.await
			.unwrap()
			.unwrap();
		assert_eq!(auth.api_key.as_deref(), Some("refreshed-1"));
		let _ = std::fs::remove_dir_all(_dir);
	}
}
