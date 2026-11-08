//! OAuth state stores and providers — port of
//! `pi/packages/mcp/src/oauth/provider.ts` plus the coding-agent halves this
//! port folds into pi-mcp (`extensions/mcp/oauth.ts`): the durable
//! `mcp-auth.json` store and the transport-side token provider.
//!
//! Upstream splits these across two packages because the TS `McpOAuthProvider`
//! lives in pi-mcp while `McpOAuthCredentialStore` (the `mcp-auth.json`
//! plumbing) and `createMcpAuthProvider` live in the coding agent. pi-rs
//! keeps the whole stack here so the transport and its auth provider share
//! one crate; the coding-agent assembly simply constructs a
//! [`StoredTokenProvider`].
//!
//! Declared differences from the TS original:
//! - The Client ID Metadata Document *builder* (upstream serves one from
//!   pi.dev) is not ported; the flow accepts a document through the
//!   provider hook but pi-rs ships none.
//! - Settings secrets are resolved before construction (pi-rs resolves
//!   `${VAR}`/`!cmd` config values at config load), while upstream resolves
//!   them lazily at refresh time.
//! - The multi-process refresh lock uses a `mkdir` lock with upstream's
//!   parameters (20s stale, 25s wait, 100ms retry) instead of
//!   `proper-lockfile`, and the per-write file lock mirrors the
//!   `mkdir` stand-in pi-ai's auth.json store already uses (30s).

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;

use crate::auth_provider::AuthProvider;
use crate::auth_provider::UnauthorizedContext;
use crate::oauth::discovery::parse_www_authenticate;
use crate::oauth::errors::Error as OAuthError;
use crate::oauth::flow::{ClientMetadata, FlowOptions, FlowResult, authorize};
use crate::oauth::types::{
	ClientInformation, ClientMetadataDocument, DiscoveryState, InvalidateKind, OAuthState,
	OAuthTokens,
};

/// Where a loopback callback server would listen when a fixed redirect was
/// not registered — refreshes never redirect the user (upstream
/// `FALLBACK_REDIRECT_URL`).
const FALLBACK_REDIRECT_URL: &str = "http://127.0.0.1/callback";
/// Access tokens this close to expiry are refreshed before they are sent
/// (upstream `REFRESH_SKEW_MS`).
const REFRESH_SKEW_MS: i64 = 30_000;
/// Bounds each request to the authorization server, so an unresponsive one
/// cannot hold the refresh lock or delay shutdown (upstream
/// `OAUTH_REQUEST_TIMEOUT_MS`).
const OAUTH_REQUEST_TIMEOUT_SECS: u64 = 15;
/// A refresh lock that its holder stopped renewing (the process was killed)
/// is taken over after this (upstream `REFRESH_LOCK_STALE_MS`).
const REFRESH_LOCK_STALE_MS: u64 = 20_000;
/// How long to wait for another process's refresh: longer than a stale lock
/// lives (upstream `REFRESH_LOCK_WAIT_MS`).
const REFRESH_LOCK_WAIT_MS: u64 = 25_000;
const REFRESH_LOCK_RETRY_MS: u64 = 100;

fn oauth_client() -> reqwest::Client {
	reqwest::Client::builder()
		.timeout(Duration::from_secs(OAUTH_REQUEST_TIMEOUT_SECS))
		.build()
		.expect("reqwest client builds")
}

/// Milliseconds since the epoch — the `tokensExpireAt` clock (shared with
/// flow tests).
pub(crate) fn now_ms() -> i64 {
	std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map(|duration| duration.as_millis() as i64)
		.unwrap_or(0)
}

/// Random bytes from the OS CSPRNG (shared helper of the pi-ai auth port).
fn random_bytes(buffer: &mut [u8]) -> Result<(), OAuthError> {
	pi_ai::auth::pkce::getrandom_fill(buffer)
		.map_err(|error| OAuthError::Invalid(format!("random source failed: {error}")))
}

/// Persisted per-server OAuth state — upstream `McpOAuthStateStore`.
#[async_trait]
pub trait OAuthStateStore: Send + Sync {
	/// The stored state of one server, `None` when nothing is stored.
	async fn load(&self) -> Result<Option<OAuthState>, String>;
	/// Replace the stored state of one server.
	async fn save(&self, state: OAuthState) -> Result<(), String>;
}

/// In-process store for tests — upstream `MemoryOAuthStateStore`.
#[derive(Debug, Default)]
pub struct MemoryStateStore {
	state: Mutex<Option<OAuthState>>,
}

impl MemoryStateStore {
	pub fn new() -> Self {
		Self::default()
	}

	/// Seed a state directly (the "pre-seeded token" entry path).
	pub fn seed(&self, state: OAuthState) {
		*self.state.lock().expect("state mutex") = Some(state);
	}
}

#[async_trait]
impl OAuthStateStore for MemoryStateStore {
	async fn load(&self) -> Result<Option<OAuthState>, String> {
		Ok(self.state.lock().expect("state mutex").clone())
	}

	async fn save(&self, state: OAuthState) -> Result<(), String> {
		*self.state.lock().expect("state mutex") = Some(state);
		Ok(())
	}
}

/// One server's slice of `mcp-auth.json` — the value bound to the key
/// `mcp__<server>|<url>` (with the legacy URL-only key taken over on first
/// load, like upstream `McpOAuthCredentialStore.forServer`).
struct ServerFileStore {
	path: PathBuf,
	key: String,
	legacy_key: String,
}

/// `mcp__<server>` with `-` replaced by `_`, like the tool names — upstream
/// `mcpNamespace` (kept byte-identical so the same file round-trips with a
/// TS pi install).
fn mcp_namespace(server: &str) -> String {
	format!("mcp__{}", server.replace('-', "_"))
}

/// Keys of a server's state: by name and URL, so servers sharing a URL keep
/// separate accounts, and the legacy key by URL alone (upstream
/// `storeKeys`).
fn store_keys(name: &str, server_url: &str) -> (String, String) {
	let normalized = reqwest::Url::parse(server_url)
		.map(|url| url.to_string())
		.unwrap_or_else(|_| server_url.to_string());
	(format!("{}|{normalized}", mcp_namespace(name)), normalized)
}

/// File-backed store for the per-server OAuth states of
/// `<agent-dir>/mcp-auth.json` — a JSON object keyed by server. Writes go
/// through a `mkdir` lock next to the file (same parameters as pi-ai's
/// auth.json store) into a 0600 temp file that atomically replaces the
/// target, so concurrent CLI processes never clobber each other's tokens.
pub struct FileStateStore {
	path: PathBuf,
}

impl FileStateStore {
	/// Store backed by `path` (typically `<agent-dir>/mcp-auth.json`).
	pub fn new(path: impl Into<PathBuf>) -> Self {
		Self { path: path.into() }
	}

	/// The store of one server: load/save operate on that server's key and
	/// the first load migrates a legacy URL-keyed entry.
	pub fn server(&self, name: &str, server_url: &str) -> std::sync::Arc<dyn OAuthStateStore> {
		let (key, legacy_key) = store_keys(name, server_url);
		std::sync::Arc::new(ServerFileStore {
			path: self.path.clone(),
			key,
			legacy_key,
		})
	}

	/// The stored tokens of a server (upstream `tokens`), for noticing
	/// sign-ins done by another process.
	pub async fn tokens(
		&self,
		name: &str,
		server_url: &str,
	) -> Result<Option<OAuthTokens>, String> {
		let (key, legacy_key) = store_keys(name, server_url);
		let _guard = acquire_file_lock(&self.path).await?;
		let states = read_states(&self.path)?;
		Ok(states
			.get(&key)
			.or_else(|| states.get(&legacy_key))
			.and_then(|value| serde_json::from_value::<OAuthState>(value.clone()).ok())
			.and_then(|state| state.tokens))
	}

	/// Remove a server's state; returns whether credentials were stored
	/// (upstream `remove`).
	pub async fn remove(&self, name: &str, server_url: &str) -> Result<bool, String> {
		let (key, legacy_key) = store_keys(name, server_url);
		let _guard = acquire_file_lock(&self.path).await?;
		let mut states = read_states(&self.path)?;
		let stored = if states.contains_key(&key) {
			Some(key)
		} else if states.contains_key(&legacy_key) {
			Some(legacy_key)
		} else {
			None
		};
		let Some(stored) = stored else {
			return Ok(false);
		};
		states.remove(&stored);
		write_states(&self.path, &states)?;
		Ok(true)
	}
}

#[async_trait]
impl OAuthStateStore for ServerFileStore {
	async fn load(&self) -> Result<Option<OAuthState>, String> {
		let _guard = acquire_file_lock(&self.path).await?;
		let mut states = read_states(&self.path)?;
		// The first server to load legacy state takes it over; others with
		// the same URL sign in again.
		if !states.contains_key(&self.key)
			&& let Some(legacy) = states.remove(&self.legacy_key)
		{
			states.insert(self.key.clone(), legacy);
			write_states(&self.path, &states)?;
		}
		Ok(states
			.get(&self.key)
			.and_then(|value| serde_json::from_value::<OAuthState>(value.clone()).ok()))
	}

	async fn save(&self, state: OAuthState) -> Result<(), String> {
		let _guard = acquire_file_lock(&self.path).await?;
		let mut states = read_states(&self.path)?;
		let encoded = serde_json::to_value(&state).map_err(|error| error.to_string())?;
		states.insert(self.key.clone(), encoded);
		write_states(&self.path, &states)?;
		Ok(())
	}
}

/// Parse the states map; an absent file is empty, anything else that does
/// not parse is an error (upstream `parseStates` propagates JSON.parse
/// failures).
fn read_states(path: &Path) -> Result<serde_json::Map<String, serde_json::Value>, String> {
	let text = match std::fs::read_to_string(path) {
		Ok(text) => text,
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Default::default()),
		Err(error) => return Err(format!("Failed to read {}: {error}", path.display())),
	};
	let trimmed = text.trim();
	if trimmed.is_empty() {
		return Ok(Default::default());
	}
	serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(trimmed)
		.map_err(|error| format!("Failed to parse {}: {error}", path.display()))
}

/// Pretty-printed with a trailing newline, mode 0600, temp file + rename —
/// upstream `serializeStates` plus the shared atomic-write pattern.
fn write_states(
	path: &Path,
	states: &serde_json::Map<String, serde_json::Value>,
) -> Result<(), String> {
	if let Some(parent) = path.parent() {
		std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
	}
	let mut body = serde_json::to_string_pretty(states).map_err(|error| error.to_string())?;
	body.push('\n');
	let temp = path.parent().unwrap_or(Path::new(".")).join(format!(
		".mcp-auth-{}-{}.tmp",
		std::process::id(),
		std::time::SystemTime::now()
			.duration_since(std::time::UNIX_EPOCH)
			.map(|duration| duration.as_nanos())
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
		std::io::Write::write_all(&mut file, body.as_bytes())?;
		file.sync_all()?;
		std::fs::rename(&temp, path)?;
		Ok(())
	})();
	if result.is_err() {
		let _ = std::fs::remove_file(&temp);
	}
	result.map_err(|error| error.to_string())
}

/// `mkdir`-based file lock with the parameters of pi-ai's auth.json store
/// (30s deadline, stale locks broken after 30s).
async fn acquire_file_lock(path: &Path) -> Result<FileLockGuard, String> {
	let lock_path = PathBuf::from(format!("{}.lock", path.display()));
	if let Some(parent) = lock_path.parent() {
		std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
	}
	let deadline = std::time::Instant::now() + Duration::from_secs(30);
	loop {
		match std::fs::create_dir(&lock_path) {
			Ok(()) => return Ok(FileLockGuard { path: lock_path }),
			Err(_) if std::time::Instant::now() >= deadline => {
				return Err(format!("Timed out waiting for the {} lock", path.display()));
			}
			Err(_) => {
				if let Ok(metadata) = std::fs::metadata(&lock_path)
					&& let Ok(modified) = metadata.modified()
					&& modified
						.elapsed()
						.is_ok_and(|age| age >= Duration::from_secs(30))
				{
					let _ = std::fs::remove_dir_all(&lock_path);
					continue;
				}
				tokio::time::sleep(Duration::from_millis(25)).await;
			}
		}
	}
}

/// Releases the `mkdir` lock on drop.
struct FileLockGuard {
	path: PathBuf,
}

impl Drop for FileLockGuard {
	fn drop(&mut self) {
		let _ = std::fs::remove_dir_all(&self.path);
	}
}

/// A per-server refresh lock: held from reading the tokens to saving new
/// ones, so two pi-rs processes never refresh with the same rotating
/// refresh token (upstream `withRefreshLock`). A lock left behind by a
/// killed process is taken over once it goes stale.
struct RefreshLockGuard {
	path: PathBuf,
}

impl Drop for RefreshLockGuard {
	fn drop(&mut self) {
		let _ = std::fs::remove_dir_all(&self.path);
	}
}

async fn acquire_refresh_lock(lock_dir: &Path, key: &str) -> Result<RefreshLockGuard, String> {
	std::fs::create_dir_all(lock_dir).map_err(|error| error.to_string())?;
	// A stable short name for the key (upstream truncates a sha256; pi-rs
	// uses an FNV-1a mix — the hash only mangles a file name).
	let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
	for byte in key.as_bytes() {
		hash ^= u64::from(*byte);
		hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
	}
	let lock_path = lock_dir.join(format!("mcp-auth-refresh-{hash:016x}"));
	let deadline = std::time::Instant::now() + Duration::from_millis(REFRESH_LOCK_WAIT_MS);
	loop {
		match std::fs::create_dir(&lock_path) {
			Ok(()) => return Ok(RefreshLockGuard { path: lock_path }),
			Err(_) if std::time::Instant::now() >= deadline => {
				return Err("Timed out waiting for the MCP OAuth refresh lock".to_string());
			}
			Err(_) => {
				if let Ok(metadata) = std::fs::metadata(&lock_path)
					&& let Ok(modified) = metadata.modified()
					&& modified
						.elapsed()
						.is_ok_and(|age| age >= Duration::from_millis(REFRESH_LOCK_STALE_MS))
				{
					let _ = std::fs::remove_dir_all(&lock_path);
					continue;
				}
				tokio::time::sleep(Duration::from_millis(REFRESH_LOCK_RETRY_MS)).await;
			}
		}
	}
}

/// Configuration of a server's OAuth client — resolved values (upstream
/// `McpOAuthSettings`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OAuthSettings {
	/// Pre-registered client id; without one the flow registers a client.
	pub client_id: Option<String>,
	/// Client secret for `client_secret_*` auth methods.
	pub client_secret: Option<String>,
	/// Fixed loopback redirect URI (a configured `callbackUrl` with a
	/// port); else the registered redirect URI is reused, else
	/// `http://127.0.0.1/callback`.
	pub redirect_url: Option<String>,
	/// Fixed callback port without a full redirect URI (a configured
	/// `callbackPort`); the redirect becomes `http://127.0.0.1:<port>/callback`.
	pub callback_port: Option<u16>,
	/// Scopes to request for the deferred interactive sign-in; the refresh
	/// path takes its scope from the server's challenge. Default: the
	/// server's advertised scopes.
	pub scope: Option<String>,
	/// `client_name` of a dynamic registration. Default: `pi-rs`.
	pub client_name: Option<String>,
	/// Metadata document to use instead of discovery, trusted as configured.
	pub auth_server_metadata_url: Option<reqwest::Url>,
}

/// Handler that receives the authorization URL when the flow needs a
/// browser (upstream `onRedirect`).
pub type OnRedirect = std::sync::Arc<dyn Fn(&reqwest::Url) + Send + Sync>;

/// Client ID Metadata Document hook (upstream
/// `OAuthClientProvider.clientMetadataDocument`): given the server metadata,
/// returns the document to identify as.
pub type ClientMetadataDocumentHook = std::sync::Arc<
	dyn Fn(
			Option<&crate::oauth::types::AuthorizationServerMetadata>,
		) -> Option<ClientMetadataDocument>
		+ Send
		+ Sync,
>;

/// Options of [`McpOAuthProvider::new`] (upstream `McpOAuthProviderOptions`).
pub struct McpOAuthProviderOptions {
	/// The exact MCP server URL the provider authorizes.
	pub server_url: String,
	/// The redirect URI of the (future) callback.
	pub redirect_url: String,
	pub client_metadata: ClientMetadata,
	/// Pre-registered client; skips dynamic registration.
	pub client_id: Option<String>,
	pub client_secret: Option<String>,
	pub store: std::sync::Arc<dyn OAuthStateStore>,
	/// Called with the authorization URL when the flow needs a browser.
	pub on_redirect: Option<OnRedirect>,
	/// Client ID Metadata Document hook (upstream
	/// `OAuthClientProvider.clientMetadataDocument`).
	pub client_metadata_document: Option<ClientMetadataDocumentHook>,
}

/// Stateful OAuth provider for one exact MCP server URL — port of upstream
/// `McpOAuthProvider`. Every mutation is a serialized read-modify-write of
/// the store, and stored state for another server URL is ignored so
/// credentials never leak across servers (`own`).
pub struct McpOAuthProvider {
	server_url: String,
	redirect_url: String,
	client_metadata: ClientMetadata,
	configured_client: Option<ClientInformation>,
	store: std::sync::Arc<dyn OAuthStateStore>,
	on_redirect: Option<OnRedirect>,
	client_metadata_document: Option<ClientMetadataDocumentHook>,
	http: reqwest::Client,
	/// Serializes read-modify-write cycles (upstream's `writes` promise
	/// chain).
	writes: tokio::sync::Mutex<()>,
}

impl McpOAuthProvider {
	pub fn new(options: McpOAuthProviderOptions) -> Self {
		let client_metadata = ClientMetadata {
			client_name: options.client_metadata.client_name,
			scope: options.client_metadata.scope,
			redirect_uris: if options.client_metadata.redirect_uris.is_empty() {
				vec![options.redirect_url.clone()]
			} else {
				options.client_metadata.redirect_uris
			},
			grant_types: if options.client_metadata.grant_types.is_empty() {
				vec![
					"authorization_code".to_string(),
					"refresh_token".to_string(),
				]
			} else {
				options.client_metadata.grant_types
			},
			response_types: if options.client_metadata.response_types.is_empty() {
				vec!["code".to_string()]
			} else {
				options.client_metadata.response_types
			},
			token_endpoint_auth_method: options.client_metadata.token_endpoint_auth_method.or_else(
				|| {
					Some(
						if options.client_secret.is_some() {
							"client_secret_post"
						} else {
							"none"
						}
						.to_string(),
					)
				},
			),
		};
		let configured_client = options.client_id.map(|client_id| ClientInformation {
			client_id,
			client_secret: options.client_secret,
			..Default::default()
		});
		Self {
			server_url: options.server_url,
			redirect_url: options.redirect_url,
			client_metadata,
			configured_client,
			store: options.store,
			on_redirect: options.on_redirect,
			client_metadata_document: options.client_metadata_document,
			http: oauth_client(),
			writes: tokio::sync::Mutex::new(()),
		}
	}

	/// The registration metadata this provider sends (upstream
	/// `provider.clientMetadata`).
	pub fn client_metadata(&self) -> &ClientMetadata {
		&self.client_metadata
	}

	/// The redirect URI of the (future) callback.
	pub fn redirect_url(&self) -> &str {
		&self.redirect_url
	}

	/// The provider's HTTP client (15s per-request timeout).
	pub fn http(&self) -> &reqwest::Client {
		&self.http
	}

	/// The `state` parameter of an authorization: reuse the stored one, or
	/// generate and persist a fresh 32-byte hex value (upstream `state`).
	pub async fn state(&self) -> Result<String, OAuthError> {
		let existing = self.load().await?.oauth_state;
		if let Some(existing) = existing {
			return Ok(existing);
		}
		let mut bytes = [0u8; 32];
		random_bytes(&mut bytes)?;
		let state = bytes
			.iter()
			.map(|byte| format!("{byte:02x}"))
			.collect::<String>();
		self.update(|value| value.oauth_state = Some(state.clone()))
			.await?;
		Ok(state)
	}

	pub async fn client_information(&self) -> Result<Option<ClientInformation>, OAuthError> {
		if self.configured_client.is_some() {
			return Ok(self.configured_client.clone());
		}
		Ok(self.load().await?.client_information)
	}

	pub async fn save_client_information(
		&self,
		information: &ClientInformation,
	) -> Result<(), OAuthError> {
		if self.configured_client.is_some() {
			return Ok(());
		}
		self.update(|value| value.client_information = Some(information.clone()))
			.await
	}

	pub async fn tokens(&self) -> Result<Option<OAuthTokens>, OAuthError> {
		Ok(self.load().await?.tokens)
	}

	pub async fn save_tokens(&self, tokens: OAuthTokens) -> Result<(), OAuthError> {
		let expires_at = tokens
			.expires_in
			.map(|expires_in| now_ms() + expires_in * 1000);
		self.update(|value| {
			value.tokens = Some(tokens.clone());
			value.tokens_expire_at = expires_at;
		})
		.await
	}

	pub async fn code_verifier(&self) -> Result<String, OAuthError> {
		self.load()
			.await?
			.code_verifier
			.ok_or_else(|| OAuthError::Invalid("No OAuth PKCE code verifier is stored".to_string()))
	}

	pub async fn save_code_verifier(&self, verifier: &str) -> Result<(), OAuthError> {
		self.update(|value| value.code_verifier = Some(verifier.to_string()))
			.await
	}

	pub async fn discovery_state(&self) -> Result<Option<DiscoveryState>, OAuthError> {
		Ok(self.load().await?.discovery)
	}

	pub async fn save_discovery_state(&self, discovery: DiscoveryState) -> Result<(), OAuthError> {
		self.update(|value| value.discovery = Some(discovery)).await
	}

	/// Clear stored credentials of one kind (upstream `invalidateCredentials`).
	pub async fn invalidate_credentials(&self, kind: InvalidateKind) -> Result<(), OAuthError> {
		self.update(|value| {
			if matches!(kind, InvalidateKind::All | InvalidateKind::Client) {
				value.client_information = None;
			}
			if matches!(kind, InvalidateKind::All | InvalidateKind::Tokens) {
				value.tokens = None;
				value.tokens_expire_at = None;
			}
			if matches!(kind, InvalidateKind::All | InvalidateKind::Verifier) {
				value.code_verifier = None;
			}
			if matches!(kind, InvalidateKind::Discovery) {
				// Upstream's "all" keeps the discovery cache; only an
				// explicit discovery invalidation clears it.
				value.discovery = None;
			}
			if matches!(kind, InvalidateKind::All) {
				value.oauth_state = None;
			}
		})
		.await
	}

	/// The Client ID Metadata Document to identify as, when the hook is set
	/// and no client information is stored (upstream
	/// `OAuthClientProvider.clientMetadataDocument`; the flow calls this).
	pub async fn client_metadata_document(
		&self,
		metadata: Option<&crate::oauth::types::AuthorizationServerMetadata>,
	) -> Option<ClientMetadataDocument> {
		let hook = self.client_metadata_document.as_ref()?;
		hook(metadata)
	}

	/// Hand the authorization URL to the configured redirect handler, or
	/// report that interactive sign-in is required (upstream
	/// `redirectToAuthorization`; the flow calls this).
	pub async fn redirect_to_authorization(&self, url: &reqwest::Url) -> Result<(), OAuthError> {
		match &self.on_redirect {
			Some(on_redirect) => {
				on_redirect(url);
				Ok(())
			}
			// The interactive browser sign-in is deferred: without a redirect
			// handler the flow can only report that a sign-in is required.
			None => Err(OAuthError::AuthorizationRequired),
		}
	}

	/// Stored state for another server URL is ignored so credentials never
	/// leak across servers (upstream `own`).
	async fn load(&self) -> Result<OAuthState, OAuthError> {
		let state = self.store.load().await.map_err(|error| {
			OAuthError::Invalid(format!("Failed to load MCP OAuth state: {error}"))
		})?;
		Ok(match state {
			Some(state) if state.server_url == self.server_url => state,
			_ => OAuthState {
				server_url: self.server_url.clone(),
				..Default::default()
			},
		})
	}

	/// Serialized load-mutate-save (upstream's `update`).
	async fn update<F>(&self, mutate: F) -> Result<(), OAuthError>
	where
		F: FnOnce(&mut OAuthState),
	{
		let _guard = self.writes.lock().await;
		let mut state = self.load().await?;
		mutate(&mut state);
		self.store.save(state).await.map_err(|error| {
			OAuthError::Invalid(format!("Failed to save MCP OAuth state: {error}"))
		})
	}
}

/// Challenge observer (upstream `onChallenge`, which feeds sign-in).
pub type ChallengeHook = std::sync::Arc<dyn Fn(&crate::oauth::types::Challenge) + Send + Sync>;

/// Transport-side auth provider for one MCP server — the port of the
/// coding-agent's `createMcpAuthProvider`. It sends the stored access token,
/// refreshes it before it expires (30s skew) and again after a 401, and
/// fails with [`OAuthError::AuthorizationRequired`] when the user has to
/// sign in (including on a `insufficient_scope` challenge, since a refresh
/// keeps the granted scope).
///
/// Many servers rotate refresh tokens, so two refreshes with the same
/// refresh token lose the grant: requests in this process share one refresh
/// (a second caller sees the rotated token and skips), and other processes
/// are kept out by the optional refresh lock, held from reading the tokens
/// to saving new ones.
pub struct StoredTokenProvider {
	server_name: String,
	server_url: String,
	store: std::sync::Arc<dyn OAuthStateStore>,
	settings: OAuthSettings,
	/// Serializes refreshes in this process (upstream's `refreshing`
	/// single-flight promise).
	refreshes: tokio::sync::Mutex<()>,
	/// Directory for the cross-process refresh lock; without one, refreshes
	/// are only serialized in this process.
	lock_dir: Option<PathBuf>,
	on_challenge: Option<ChallengeHook>,
}

impl StoredTokenProvider {
	pub fn new(
		server_name: impl Into<String>,
		server_url: impl Into<String>,
		store: std::sync::Arc<dyn OAuthStateStore>,
		settings: OAuthSettings,
	) -> Self {
		Self {
			server_name: server_name.into(),
			server_url: server_url.into(),
			store,
			settings,
			refreshes: tokio::sync::Mutex::new(()),
			lock_dir: None,
			on_challenge: None,
		}
	}

	/// Enable the cross-process refresh lock under `dir` (upstream passes
	/// the agent directory).
	pub fn with_lock_dir(mut self, dir: impl Into<PathBuf>) -> Self {
		self.lock_dir = Some(dir.into());
		self
	}

	/// Observe every challenge the server sends (upstream `onChallenge`,
	/// which feeds sign-in).
	pub fn with_on_challenge(mut self, hook: ChallengeHook) -> Self {
		self.on_challenge = Some(hook);
		self
	}

	/// Resolves when no refresh is running, so shutdown does not drop
	/// rotated tokens before they are saved (upstream `settled`).
	pub async fn settled(&self) {
		let _ = self.refreshes.lock().await;
	}

	/// The flow provider used for refreshes, bound to the same store.
	fn flow_provider(&self, redirect_url: &str) -> McpOAuthProvider {
		McpOAuthProvider::new(McpOAuthProviderOptions {
			server_url: self.server_url.clone(),
			redirect_url: redirect_url.to_string(),
			// Upstream's createProvider sends only a `client_name`; the
			// scope of a refresh comes from the server's challenge.
			client_metadata: ClientMetadata {
				client_name: Some(
					self.settings
						.client_name
						.clone()
						.unwrap_or_else(|| "pi-rs".to_string()),
				),
				..Default::default()
			},
			client_id: self.settings.client_id.clone(),
			client_secret: self.settings.client_secret.clone(),
			store: self.store.clone(),
			// A refresh never redirects the user.
			on_redirect: None,
			client_metadata_document: None,
		})
	}

	/// Replace `stale_token`, the access token that expired or was rejected.
	async fn refresh(
		&self,
		stale_token: Option<String>,
		challenge: Option<crate::oauth::types::Challenge>,
	) -> Result<(), OAuthError> {
		// Concurrent 401s share one refresh: later callers see the rotated
		// token and skip.
		let _in_flight = self.refreshes.lock().await;
		let key = store_keys(&self.server_name, &self.server_url).0;
		let _refresh_lock = match &self.lock_dir {
			Some(dir) => Some(
				acquire_refresh_lock(dir, &key)
					.await
					.map_err(OAuthError::Invalid)?,
			),
			None => None,
		};
		let state = self.store.load().await.map_err(|error| {
			OAuthError::Invalid(format!("Failed to load MCP OAuth state: {error}"))
		})?;
		let tokens = state.as_ref().and_then(|state| state.tokens.as_ref());
		// Tokens that changed meanwhile (another process refreshed them, or
		// the user signed in) are used without refreshing.
		if tokens.map(|tokens| tokens.access_token.as_str()) != stale_token.as_deref() {
			return Ok(());
		}
		let Some(tokens) = tokens else {
			return Err(OAuthError::AuthorizationRequired);
		};
		if tokens.refresh_token.is_none() {
			return Err(OAuthError::AuthorizationRequired);
		}
		let redirect_url = self
			.settings
			.redirect_url
			.clone()
			.or_else(|| {
				state
					.as_ref()
					.and_then(|state| state.client_information.as_ref())
					.and_then(|client| client.redirect_uris.as_deref())
					.and_then(|uris| uris.first().cloned())
			})
			.unwrap_or_else(|| FALLBACK_REDIRECT_URL.to_string());
		let provider = self.flow_provider(&redirect_url);
		// Refreshes the tokens, or reports that a new sign-in is needed.
		let result = authorize(
			&provider,
			FlowOptions {
				server_url: self.server_url.clone(),
				resource_metadata_url: challenge
					.as_ref()
					.and_then(|challenge| challenge.resource_metadata_url.clone()),
				authorization_server_metadata_url: self.settings.auth_server_metadata_url.clone(),
				scope: challenge
					.as_ref()
					.and_then(|challenge| challenge.scope.clone()),
				..Default::default()
			},
		)
		.await?;
		if matches!(result, FlowResult::Redirect) {
			return Err(OAuthError::AuthorizationRequired);
		}
		Ok(())
	}
}

#[async_trait]
impl AuthProvider for StoredTokenProvider {
	async fn token(&self) -> Option<String> {
		let state = self.store.load().await.ok()??;
		let token = state
			.tokens
			.as_ref()
			.map(|tokens| tokens.access_token.clone());
		let expired = state
			.tokens_expire_at
			.is_some_and(|expires_at| expires_at.saturating_sub(REFRESH_SKEW_MS) <= now_ms());
		let refreshable = state
			.tokens
			.as_ref()
			.is_some_and(|tokens| tokens.refresh_token.is_some());
		if !expired || !refreshable {
			return token;
		}
		// Failures fall through: the request goes out with the old token and
		// a 401 decides what happens.
		let _ = self.refresh(token.clone(), None).await;
		self.store
			.load()
			.await
			.ok()?
			.and_then(|state| state.tokens)
			.map(|tokens| tokens.access_token)
	}

	async fn on_unauthorized(&self, context: UnauthorizedContext) -> Result<(), OAuthError> {
		let challenge = parse_www_authenticate(context.www_authenticate.as_deref());
		if let Some(hook) = &self.on_challenge {
			hook(&challenge);
		}
		// A refresh keeps the granted scope, so more scope needs a new
		// sign-in.
		if challenge.error.as_deref() == Some("insufficient_scope") {
			return Err(OAuthError::AuthorizationRequired);
		}
		self.refresh(context.token, Some(challenge)).await
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::oauth::types::OAuthTokens;

	fn state_with_tokens(
		server_url: &str,
		access: &str,
		refresh: Option<&str>,
		expires_in: Option<i64>,
	) -> OAuthState {
		OAuthState {
			server_url: server_url.to_string(),
			tokens: Some(OAuthTokens {
				access_token: access.to_string(),
				token_type: "Bearer".to_string(),
				refresh_token: refresh.map(str::to_string),
				expires_in,
				..Default::default()
			}),
			tokens_expire_at: expires_in.map(|expires_in| now_ms() + expires_in * 1000),
			..Default::default()
		}
	}

	#[tokio::test]
	async fn file_store_round_trips_and_migrates_the_legacy_key() {
		let dir = std::env::temp_dir().join(format!(
			"pi-mcp-oauth-store-{}-{}",
			std::process::id(),
			now_ms()
		));
		let path = dir.join("mcp-auth.json");
		let store = FileStateStore::new(&path);
		let legacy = store.server("docs", "https://mcp.example.com/mcp");
		// Seed under the legacy URL-only key, the shape older versions wrote.
		std::fs::create_dir_all(&dir).unwrap();
		let (key, legacy_key) = store_keys("docs", "https://mcp.example.com/mcp");
		assert!(legacy_key.starts_with("https://mcp.example.com/mcp"));
		assert_eq!(key, format!("mcp__docs|{legacy_key}"));
		let mut states = serde_json::Map::new();
		states.insert(
			legacy_key.clone(),
			serde_json::to_value(state_with_tokens(
				"https://mcp.example.com/mcp",
				"seeded",
				None,
				None,
			))
			.unwrap(),
		);
		write_states(&path, &states).unwrap();

		// First load takes the legacy entry over.
		let loaded = legacy.load().await.unwrap().unwrap();
		assert_eq!(loaded.tokens.unwrap().access_token, "seeded");
		let raw: serde_json::Value =
			serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
		assert!(raw.get(&key).is_some() && raw.get(&legacy_key).is_none());

		// A save through the new key is a read-modify-write that keeps the
		// file's other entries, and records the expiry from `expires_in`.
		let before = now_ms();
		legacy
			.save(state_with_tokens(
				"https://mcp.example.com/mcp",
				"fresh",
				Some("rt"),
				Some(60),
			))
			.await
			.unwrap();
		let after = now_ms();
		let raw: serde_json::Value =
			serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
		assert_eq!(raw[key.as_str()]["tokens"]["refresh_token"], "rt");
		let expires_at = raw[key.as_str()]["tokensExpireAt"].as_i64().unwrap();
		assert!(
			expires_at >= before + 60_000 && expires_at <= after + 60_000,
			"expiry is save time + expires_in: {expires_at} not in [{}, {}]",
			before + 60_000,
			after + 60_000
		);
		#[cfg(unix)]
		{
			use std::os::unix::fs::PermissionsExt;
			assert_eq!(
				std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
				0o600
			);
		}
		assert!(
			store
				.tokens("docs", "https://mcp.example.com/mcp")
				.await
				.unwrap()
				.is_some()
		);
		assert!(
			store
				.remove("docs", "https://mcp.example.com/mcp")
				.await
				.unwrap()
		);
		assert!(
			!store
				.remove("docs", "https://mcp.example.com/mcp")
				.await
				.unwrap()
		);
		let _ = std::fs::remove_dir_all(dir);
	}

	#[tokio::test]
	async fn provider_ignores_state_of_other_servers() {
		let store = std::sync::Arc::new(MemoryStateStore::new());
		store.seed(OAuthState {
			server_url: "https://other.example.com/mcp".to_string(),
			tokens: Some(OAuthTokens {
				access_token: "foreign".to_string(),
				token_type: "Bearer".to_string(),
				..Default::default()
			}),
			..Default::default()
		});
		let provider = McpOAuthProvider::new(McpOAuthProviderOptions {
			server_url: "https://mcp.example.com/mcp".to_string(),
			redirect_url: "http://127.0.0.1:8192/callback".to_string(),
			client_metadata: ClientMetadata {
				client_name: Some("pi-rs".to_string()),
				..Default::default()
			},
			client_id: None,
			client_secret: None,
			store: store.clone(),
			on_redirect: None,
			client_metadata_document: None,
		});
		assert!(provider.tokens().await.unwrap().is_none());
		// Defaults apply: redirect URIs, grant/response types, public client.
		assert_eq!(
			provider.client_metadata().redirect_uris,
			vec!["http://127.0.0.1:8192/callback".to_string()]
		);
		assert_eq!(
			provider.client_metadata().grant_types,
			vec![
				"authorization_code".to_string(),
				"refresh_token".to_string()
			]
		);
		assert_eq!(
			provider.client_metadata().response_types,
			vec!["code".to_string()]
		);
		assert_eq!(
			provider
				.client_metadata()
				.token_endpoint_auth_method
				.as_deref(),
			Some("none")
		);
	}

	#[tokio::test]
	async fn state_is_stable_once_generated() {
		let store = std::sync::Arc::new(MemoryStateStore::new());
		let provider = McpOAuthProvider::new(McpOAuthProviderOptions {
			server_url: "https://mcp.example.com/mcp".to_string(),
			redirect_url: "http://127.0.0.1:8192/callback".to_string(),
			client_metadata: ClientMetadata::default(),
			client_id: None,
			client_secret: None,
			store: store.clone(),
			on_redirect: None,
			client_metadata_document: None,
		});
		let state = provider.state().await.unwrap();
		assert_eq!(state.len(), 64, "32 random bytes hex-encoded");
		assert_eq!(provider.state().await.unwrap(), state);
	}

	// -- StoredTokenProvider + StreamableHttpTransport full chain ------------

	use std::sync::Arc;

	use crate::Transport as _;
	use crate::TransportEvent;
	use crate::protocol::jsonrpc::JsonRpcRequest;
	use crate::test_support::{self, MockRequest, MockResponse};
	use crate::transports::streamable_http::StreamableHttpTransport;

	/// Seed `mcp-auth.json` with one server's state, upstream's camelCase
	/// shape.
	fn seed_auth_file(
		path: &Path,
		server_url: &str,
		tokens: serde_json::Value,
		expires_at: Option<i64>,
	) {
		let key = format!("mcp__chain|{server_url}");
		let mut state = serde_json::json!({
			"serverUrl": server_url,
			"clientInformation": {
				"client_id": "client-1",
				"redirect_uris": ["http://127.0.0.1:8192/callback"]
			},
			"tokens": tokens
		});
		if let Some(expires_at) = expires_at {
			state["tokensExpireAt"] = serde_json::json!(expires_at);
		}
		std::fs::create_dir_all(path.parent().unwrap()).unwrap();
		std::fs::write(path, serde_json::json!({key: state}).to_string()).unwrap();
	}

	fn seeded_tokens(access: &str, refresh: &str) -> serde_json::Value {
		serde_json::json!({
			"access_token": access,
			"token_type": "Bearer",
			"refresh_token": refresh
		})
	}

	/// One origin serving both the MCP endpoint and its OAuth endpoints:
	/// `/mcp` answers 200 only for `good_bearer`, `/token` answers a rotated
	/// refresh grant. Returns the base URL (with trailing slash) and a log
	/// of every raw request.
	fn chain_mock(
		connections: usize,
		good_bearer: &'static str,
		mcp_reject: Arc<dyn Fn(&mut MockResponseBuilder) + Send + Sync>,
	) -> (
		String,
		Arc<std::sync::Mutex<Vec<String>>>,
		std::thread::JoinHandle<()>,
	) {
		let log: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
		let handler_log = log.clone();
		let (base, handle) = test_support::serve(
			connections,
			Arc::new(move |request: &MockRequest| {
				handler_log
					.lock()
					.unwrap()
					.push(request.raw.to_ascii_lowercase());
				match (request.method.as_str(), request.path.as_str()) {
					("POST", "/mcp") | ("POST", "/") => {
						let authorized = request
							.raw
							.to_ascii_lowercase()
							.contains(&format!("authorization: bearer {good_bearer}"));
						if authorized {
							MockResponse::json(
								"200 OK",
								r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#,
							)
						} else {
							let host = request
								.raw
								.split("\r\n")
								.find_map(|line| {
									let (name, value) = line.split_once(':')?;
									name.eq_ignore_ascii_case("host")
										.then(|| value.trim().to_string())
								})
								.unwrap_or_default();
							let mut builder = MockResponseBuilder::default();
							mcp_reject(&mut builder);
							if builder.status.is_empty() {
								builder.status = "401 Unauthorized";
								builder.challenge = Some(format!(
									"Bearer resource_metadata=\"http://{host}/.well-known/oauth-protected-resource\""
								));
							}
							MockResponse {
								status: builder.status,
								headers: builder.headers(),
								body: "denied".to_string(),
							}
						}
					}
					("GET", "/.well-known/oauth-protected-resource") => {
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
								"code_challenge_methods_supported": ["S256"]
							})
							.to_string(),
						)
					}
					("POST", "/token") => MockResponse::json(
						"200 OK",
						serde_json::json!({
							"access_token": good_bearer,
							"token_type": "Bearer",
							"expires_in": 3600,
							"refresh_token": "rt-new"
						})
						.to_string(),
					),
					other => panic!("unexpected request {other:?}"),
				}
			}),
		);
		(format!("{base}/"), log, handle)
	}

	#[derive(Default)]
	struct MockResponseBuilder {
		status: &'static str,
		challenge: Option<String>,
	}

	impl MockResponseBuilder {
		fn headers(&self) -> String {
			match &self.challenge {
				Some(challenge) => format!("WWW-Authenticate: {challenge}\r\n"),
				None => String::new(),
			}
		}
	}

	fn chain_transport(server_url: &str, auth_path: &Path) -> StreamableHttpTransport {
		let store = FileStateStore::new(auth_path);
		let provider = StoredTokenProvider::new(
			"chain",
			server_url,
			store.server("chain", server_url),
			OAuthSettings::default(),
		);
		StreamableHttpTransport::new(server_url, Vec::new())
			.with_auth_provider(std::sync::Arc::new(provider))
	}

	async fn chain_send(transport: &mut StreamableHttpTransport) -> crate::Result<()> {
		transport
			.send(&JsonRpcRequest::request(
				1,
				"tools/list",
				serde_json::Value::Null,
			))
			.await
	}

	#[tokio::test]
	async fn transport_401_triggers_refresh_and_retries_with_the_new_token() {
		let dir = std::env::temp_dir().join(format!(
			"pi-mcp-chain-401-{}-{}",
			std::process::id(),
			now_ms()
		));
		let auth_path = dir.join("mcp-auth.json");
		let (server_url, log, handle) = chain_mock(5, "refreshed-token", Arc::new(|_builder| {}));
		seed_auth_file(
			&auth_path,
			&server_url,
			seeded_tokens("seeded-token", "rt-seeded"),
			Some(now_ms() + 3_600_000),
		);
		let mut transport = chain_transport(&server_url, &auth_path);
		chain_send(&mut transport).await.unwrap();
		match tokio::time::timeout(std::time::Duration::from_secs(3), transport.recv())
			.await
			.unwrap()
			.unwrap()
			.unwrap()
		{
			TransportEvent::Message(frame) => assert_eq!(frame.result.unwrap()["ok"], true),
			other => panic!("unexpected event: {other:?}"),
		}
		let log = log.lock().unwrap().clone();
		assert_eq!(
			log.len(),
			5,
			"mcp 401, protected resource, AS metadata, token, mcp 200: {log:?}"
		);
		assert!(
			log[0].contains("authorization: bearer seeded-token"),
			"{log:?}"
		);
		assert!(log[0].starts_with("post /"), "{log:?}");
		assert!(
			log[1].starts_with("get /.well-known/oauth-protected-resource"),
			"{log:?}"
		);
		assert!(
			log[2].starts_with("get /.well-known/oauth-authorization-server"),
			"{log:?}"
		);
		assert!(log[3].contains("grant_type=refresh_token"), "{log:?}");
		assert!(log[3].contains("refresh_token=rt-seeded"), "{log:?}");
		assert!(
			log[4].contains("authorization: bearer refreshed-token"),
			"{log:?}"
		);
		// The rotated refresh token was persisted for the next refresh.
		let store = FileStateStore::new(&auth_path);
		let state = store
			.server("chain", &server_url)
			.load()
			.await
			.unwrap()
			.unwrap();
		assert_eq!(
			state.tokens.as_ref().unwrap().access_token,
			"refreshed-token"
		);
		assert_eq!(
			state.tokens.as_ref().unwrap().refresh_token.as_deref(),
			Some("rt-new")
		);
		transport.close().await.unwrap();
		handle.join().unwrap();
		let _ = std::fs::remove_dir_all(dir);
	}

	#[tokio::test]
	async fn transport_refreshes_proactively_before_expiry() {
		let dir = std::env::temp_dir().join(format!(
			"pi-mcp-chain-expired-{}-{}",
			std::process::id(),
			now_ms()
		));
		let auth_path = dir.join("mcp-auth.json");
		let (server_url, log, handle) = chain_mock(4, "refreshed-token", Arc::new(|_builder| {}));
		// Expired 30 seconds ago (past the refresh skew).
		seed_auth_file(
			&auth_path,
			&server_url,
			seeded_tokens("stale-token", "rt-seeded"),
			Some(now_ms() - 31_000),
		);
		let mut transport = chain_transport(&server_url, &auth_path);
		chain_send(&mut transport).await.unwrap();
		let log = log.lock().unwrap().clone();
		assert_eq!(
			log.len(),
			4,
			"no 401: the refresh happened before the first request: {log:?}"
		);
		assert!(
			log[0].starts_with("get /.well-known/oauth-protected-resource"),
			"{log:?}"
		);
		assert!(
			log[3].contains("authorization: bearer refreshed-token"),
			"{log:?}"
		);
		transport.close().await.unwrap();
		handle.join().unwrap();
		let _ = std::fs::remove_dir_all(dir);
	}

	#[tokio::test]
	async fn insufficient_scope_challenge_demands_interactive_sign_in() {
		let dir = std::env::temp_dir().join(format!(
			"pi-mcp-chain-scope-{}-{}",
			std::process::id(),
			now_ms()
		));
		let auth_path = dir.join("mcp-auth.json");
		let (server_url, _log, handle) = chain_mock(
			1,
			"never-matched",
			Arc::new(|builder| {
				builder.status = "403 Forbidden";
				builder.challenge =
					Some("Bearer error=\"insufficient_scope\", scope=\"admin\"".to_string());
			}),
		);
		seed_auth_file(
			&auth_path,
			&server_url,
			seeded_tokens("seeded-token", "rt-seeded"),
			Some(now_ms() + 3_600_000),
		);
		let mut transport = chain_transport(&server_url, &auth_path);
		let error = chain_send(&mut transport).await.unwrap_err();
		assert!(
			error.to_string().contains("user interaction"),
			"a refresh keeps the granted scope, so more scope needs a sign-in: {error}"
		);
		transport.close().await.unwrap();
		handle.join().unwrap();
		let _ = std::fs::remove_dir_all(dir);
	}

	#[tokio::test]
	async fn missing_credentials_report_that_a_sign_in_is_required() {
		let dir = std::env::temp_dir().join(format!(
			"pi-mcp-chain-empty-{}-{}",
			std::process::id(),
			now_ms()
		));
		let auth_path = dir.join("mcp-auth.json");
		let (server_url, log, handle) = chain_mock(1, "never-matched", Arc::new(|_builder| {}));
		let mut transport = chain_transport(&server_url, &auth_path);
		let error = chain_send(&mut transport).await.unwrap_err();
		assert!(error.to_string().contains("user interaction"), "{error}");
		// The request went out unauthenticated and was rejected.
		let log = log.lock().unwrap().clone();
		assert!(log[0].starts_with("post /"), "{log:?}");
		assert!(!log[0].contains("authorization: bearer"));
		transport.close().await.unwrap();
		handle.join().unwrap();
		let _ = std::fs::remove_dir_all(dir);
	}
}
