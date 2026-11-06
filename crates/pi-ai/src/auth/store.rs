//! Credential storage — port of `pi/packages/coding-agent/src/core/
//! auth-storage.ts` (the `AuthStorage`/`FileAuthStorageBackend` halves) as a
//! single file-backed store plus an in-memory variant for tests.
//!
//! The file is `auth.json`: `{ "<providerId>": Credential, ... }`, written
//! pretty-printed with mode 0600. Writes serialize through a `mkdir`-based
//! lock file next to the target (the Rust stand-in for upstream's
//! `proper-lockfile`), so concurrent CLI/TUI processes refresh tokens without
//! double-rotating them. Declared difference: no Windows EPERM retry ladder
//! and no `sharedAuthFileReadState` cross-instance cache — every read hits
//! the file (pi-rs keeps no long-lived AuthStorage instances).

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::types::{AuthError, Credential, CredentialInfo};

/// `read` resolves `undefined` for missing entries and only fails on real
/// storage errors; parse/validation failures surface with upstream messages.
#[derive(Debug, Clone)]
pub struct AuthStorage {
	auth_path: PathBuf,
}

impl AuthStorage {
	/// Store backed by `auth_path` (typically `<agentDir>/auth.json`).
	pub fn new(auth_path: impl Into<PathBuf>) -> Self {
		Self {
			auth_path: auth_path.into(),
		}
	}

	pub fn auth_path(&self) -> &Path {
		&self.auth_path
	}

	/// Read the stored credential (raw — configured key values are *not*
	/// resolved here; the coding-agent layer applies its `$ENV`/`!cmd`
	/// resolution when building request auth).
	pub fn read(&self, provider: &str) -> Result<Option<Credential>, String> {
		let data = self.load_data()?;
		Ok(data.get(provider).cloned())
	}

	/// Non-secret credential metadata without resolving configured values.
	pub fn list(&self) -> Result<Vec<CredentialInfo>, String> {
		let data = self.load_data()?;
		Ok(data
			.into_iter()
			.map(|(provider_id, credential)| CredentialInfo {
				provider_id,
				r#type: credential.type_label().to_string(),
			})
			.collect())
	}

	/// Serialized read-modify-write — the only write path (`modify`). The
	/// closure sees the current credential because correct writes (refresh,
	/// login-during-refresh) depend on it; the returned credential replaces
	/// the entry (`None` removes it). Closure rejections propagate (upstream:
	/// "Rejections from `fn` propagate"); storage failures surface as the
	/// outer `Err`.
	pub async fn modify<F, Fut, T, E>(
		&self,
		provider: &str,
		f: F,
	) -> Result<Result<Option<T>, E>, String>
	where
		F: FnOnce(Option<Credential>) -> Fut,
		E: std::fmt::Display,
		Fut: std::future::Future<Output = Result<(Option<T>, Option<Credential>), E>> + Send,
	{
		let _guard = self.acquire_lock_async().await.map_err(|e| {
			AuthError::StoreModify {
				provider: provider.to_string(),
				message: e,
			}
			.to_string()
		})?;
		let mut data = self.load_data()?;
		let current = data.get(provider).cloned();
		let (result, next) = f(current).await.map_err(|e| e.to_string())?;
		match next {
			Some(credential) => {
				data.insert(provider.to_string(), credential);
			}
			None => {
				data.remove(provider);
			}
		}
		self.write_data(&data)?;
		Ok(Ok(result))
	}

	/// Remove a credential (logout), serialized against [`AuthStorage::modify`].
	pub async fn delete(&self, provider: &str) -> Result<(), String> {
		self.modify::<_, _, (), std::convert::Infallible>(provider, |_current| async {
			Ok((None, None))
		})
		.await
		.map(|inner| inner.map(|_| ()))
		.and_then(|outer| outer.map_err(|e| e.to_string()))
	}

	/// One-off synchronous read without resolving configured values
	/// (upstream `readStoredCredential`).
	pub fn read_stored_credential(auth_path: &Path, provider_id: &str) -> Option<Credential> {
		let text = std::fs::read_to_string(auth_path).ok()?;
		let value: serde_json::Value = serde_json::from_str(strip_bom(&text)).ok()?;
		value
			.get(provider_id)
			.and_then(|raw| serde_json::from_value::<Credential>(raw.clone()).ok())
	}

	// -- file plumbing ------------------------------------------------------

	fn load_data(&self) -> Result<BTreeMap<String, Credential>, String> {
		let text = match std::fs::read_to_string(&self.auth_path) {
			Ok(text) => text,
			Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
			Err(e) => return Err(format!("Failed to read auth.json: {e}")),
		};
		parse_auth_data(strip_bom(&text))
	}

	fn write_data(&self, data: &BTreeMap<String, Credential>) -> Result<(), String> {
		if let Some(parent) = self.auth_path.parent() {
			std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
		}
		// Write via a 0600 temp file + rename, like models.json writes.
		let temp = self
			.auth_path
			.parent()
			.unwrap_or(Path::new("."))
			.join(format!(
				".auth-{}-{}.tmp",
				std::process::id(),
				std::time::SystemTime::now()
					.duration_since(std::time::UNIX_EPOCH)
					.map(|d| d.as_nanos())
					.unwrap_or(0)
			));
		let result = (|| -> std::io::Result<()> {
			let mut options = std::fs::OpenOptions::new();
			options.write(true).create_new(true);
			#[cfg(unix)]
			{
				use std::os::unix::fs::OpenOptionsExt;
				options.mode(0o600);
			}
			let mut file = options.open(&temp)?;
			serde_json::to_writer_pretty(&mut file, data)?;
			file.write_all(b"\n")?;
			file.sync_all()?;
			std::fs::rename(&temp, &self.auth_path)?;
			Ok(())
		})();
		if result.is_err() {
			let _ = std::fs::remove_file(&temp);
		}
		result.map_err(|e| e.to_string())
	}

	// -- lock ---------------------------------------------------------------

	/// `mkdir`-based lock with upstream's async parameters: 30s deadline,
	/// exponential backoff (10ms base, 2s cap, jitter), stale locks older
	/// than 30s are broken.
	async fn acquire_lock_async(&self) -> Result<LockGuard, String> {
		let lock_path = self.lock_path();
		if let Some(parent) = lock_path.parent() {
			std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
		}
		let deadline = Instant::now() + Duration::from_secs(30);
		let mut retry: u32 = 0;
		loop {
			match std::fs::create_dir(&lock_path) {
				Ok(()) => return Ok(LockGuard { path: lock_path }),
				Err(_) if Instant::now() >= deadline => {
					return Err("Timed out waiting for the auth.json lock".to_string());
				}
				Err(_) => {
					// A lock left behind by a crashed process is broken once
					// it is older than the 30s stale window.
					if let Ok(metadata) = std::fs::metadata(&lock_path)
						&& let Ok(modified) = metadata.modified()
						&& modified
							.elapsed()
							.is_ok_and(|age| age >= Duration::from_secs(30))
					{
						let _ = std::fs::remove_dir_all(&lock_path);
						continue;
					}
					let base =
						Duration::from_millis((10u64.saturating_mul(1 << retry.min(8))).min(1000));
					let jitter = base.mul_f64(0.5 + fastrand());
					tokio::time::sleep(jitter.min(Duration::from_secs(2))).await;
					retry += 1;
				}
			}
		}
	}

	fn lock_path(&self) -> PathBuf {
		PathBuf::from(format!("{}.lock", self.auth_path.display()))
	}
}

/// Releases the `mkdir` lock on drop.
struct LockGuard {
	path: PathBuf,
}

impl Drop for LockGuard {
	fn drop(&mut self) {
		let _ = std::fs::remove_dir_all(&self.path);
	}
}

/// Parse + validate an auth.json body with the upstream error messages
/// (`ReadOnlyAuthStorage.load`).
pub fn parse_auth_data(text: &str) -> Result<BTreeMap<String, Credential>, String> {
	let parsed: serde_json::Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
	let object = parsed
		.as_object()
		.ok_or_else(|| "Invalid auth.json: expected an object".to_string())?;
	let mut data = BTreeMap::new();
	for (provider_id, credential) in object {
		let credential: Credential = serde_json::from_value::<Credential>(credential.clone())
			.map_err(|_| format!("Invalid auth.json credential for provider \"{provider_id}\""))?;
		data.insert(provider_id.clone(), credential);
	}
	Ok(data)
}

fn strip_bom(text: &str) -> &str {
	text.strip_prefix('\u{feff}').unwrap_or(text)
}

/// In-process randomness source for lock jitter (0.0..1.0).
fn fastrand() -> f64 {
	// SplitMix64 over a time-seeded state — no external dependency needed for
	// jitter-quality randomness.
	use std::time::{SystemTime, UNIX_EPOCH};
	let mut state = SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map(|d| d.as_nanos() as u64)
		.unwrap_or(0x9E3779B97F4A7C15)
		^ (std::process::id() as u64) << 32;
	state = state.wrapping_add(0x9E3779B97F4A7C15);
	let mut z = state;
	z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
	z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
	z ^= z >> 31;
	(z >> 11) as f64 / (1u64 << 53) as f64
}

/// In-memory store for tests — same surface, no file.
#[derive(Debug, Default)]
pub struct InMemoryAuthStorage {
	data: std::sync::Mutex<BTreeMap<String, Credential>>,
}

impl InMemoryAuthStorage {
	pub fn new() -> Self {
		Self::default()
	}

	pub fn read(&self, provider: &str) -> Option<Credential> {
		self.data.lock().unwrap().get(provider).cloned()
	}

	pub fn insert(&self, provider: &str, credential: Credential) {
		self.data
			.lock()
			.unwrap()
			.insert(provider.to_string(), credential);
	}

	pub fn list(&self) -> Vec<CredentialInfo> {
		self.data
			.lock()
			.unwrap()
			.iter()
			.map(|(provider_id, credential)| CredentialInfo {
				provider_id: provider_id.clone(),
				r#type: credential.type_label().to_string(),
			})
			.collect()
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::auth::types::OAuthCredential;
	use std::collections::BTreeMap;
	use std::convert::Infallible;

	fn temp_dir(name: &str) -> PathBuf {
		let dir = std::env::temp_dir().join(format!("pi-auth-store-{name}-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		std::fs::create_dir_all(&dir).unwrap();
		dir
	}

	fn oauth_credential(access: &str, expires_in_s: i64) -> Credential {
		let now = std::time::SystemTime::now()
			.duration_since(std::time::UNIX_EPOCH)
			.map(|d| d.as_millis() as i64)
			.unwrap_or(0);
		let mut extra = BTreeMap::new();
		extra.insert("accountId".to_string(), serde_json::json!("acc-123"));
		Credential::OAuth {
			access: access.to_string(),
			refresh: "rt".to_string(),
			expires: now + expires_in_s * 1000,
			extra,
		}
	}

	#[test]
	fn round_trip_preserves_oauth_extra_fields() {
		let dir = temp_dir("round-trip");
		let path = dir.join("auth.json");
		let store = AuthStorage::new(&path);
		let runtime = tokio::runtime::Builder::new_current_thread()
			.build()
			.unwrap();
		runtime.block_on(async {
			store
				.modify::<_, _, (), Infallible>("anthropic", |_current| async {
					Ok((None, Some(oauth_credential("sk-ant-oat-1", 3600))))
				})
				.await
				.unwrap()
				.unwrap();
			let read = store.read("anthropic").unwrap().unwrap();
			let Credential::OAuth { access, extra, .. } = read else {
				panic!("expected oauth credential");
			};
			assert_eq!(access, "sk-ant-oat-1");
			assert_eq!(extra.get("accountId"), Some(&serde_json::json!("acc-123")));
			// The written file keeps the upstream shape (type tag + extras).
			let raw: serde_json::Value =
				serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
			assert_eq!(raw["anthropic"]["type"], "oauth");
			assert_eq!(raw["anthropic"]["accountId"], "acc-123");
			#[cfg(unix)]
			{
				use std::os::unix::fs::PermissionsExt;
				assert_eq!(
					std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
					0o600
				);
			}
			// delete removes the entry.
			store.delete("anthropic").await.unwrap();
			assert!(store.read("anthropic").unwrap().is_none());
		});
		let _ = std::fs::remove_dir_all(dir);
	}

	#[test]
	fn missing_file_reads_empty_and_corrupt_reports_upstream_messages() {
		let dir = temp_dir("errors");
		let store = AuthStorage::new(dir.join("absent.json"));
		assert!(store.read("openai").unwrap().is_none());
		assert!(store.list().unwrap().is_empty());

		let path = dir.join("auth.json");
		std::fs::write(&path, "[1,2]").unwrap();
		let store = AuthStorage::new(&path);
		assert_eq!(
			store.read("openai").unwrap_err(),
			"Invalid auth.json: expected an object"
		);
		std::fs::write(&path, r#"{"x": {"type": "bogus"}}"#).unwrap();
		assert_eq!(
			store.read("x").unwrap_err(),
			"Invalid auth.json credential for provider \"x\""
		);
		let _ = std::fs::remove_dir_all(dir);
	}

	#[test]
	fn api_key_credential_env_resolution_shape() {
		// The raw store keeps `key` verbatim; config-value resolution happens
		// in the AuthContext layer.
		let mut env = BTreeMap::new();
		env.insert("CF_ACCOUNT".to_string(), "acc".to_string());
		let credential = Credential::ApiKey {
			key: Some(" literal ".to_string()),
			env: Some(env),
		};
		let raw = serde_json::to_value(&credential).unwrap();
		assert_eq!(raw["type"], "api_key");
		assert_eq!(raw["key"], " literal ");
		assert_eq!(raw["env"]["CF_ACCOUNT"], "acc");
		let parsed: Credential = serde_json::from_value(raw).unwrap();
		assert_eq!(parsed, credential);
	}

	#[test]
	fn in_memory_store_mirrors_the_file_surface() {
		let store = InMemoryAuthStorage::new();
		assert!(store.read("x").is_none());
		store.insert("x", oauth_credential("tok", 60));
		assert_eq!(store.read("x").unwrap().as_oauth().unwrap().access, "tok");
		assert_eq!(
			store.list(),
			vec![CredentialInfo {
				provider_id: "x".to_string(),
				r#type: "oauth".to_string(),
			}]
		);
	}

	// Silence the unused warning for the shared test helper type.
	#[allow(unused)]
	fn _touch(_c: &OAuthCredential) {}
}
