//! `pi-rs auth` subcommand — port of upstream `cli/auth-command.ts` +
//! `cli/auth-check.ts` + `cli/credential-print.ts`.
//!
//! Credential model: auth.json (`core::auth_storage`) first — a stored
//! credential **owns** the provider (upstream resolve.ts rule); then the
//! ambient chain, which in pi-rs is the models.json `apiKey` value
//! (literal/`$ENV`/`!cmd`) plus the adapter env vars (`ANTHROPIC_API_KEY`,
//! `OPENAI_API_KEY`, ...). Declared differences from upstream:
//! - `check` resolves `ready`/`not_ready`/`invalid` with the upstream reasons;
//!   ambient sources are the pi-rs ones above (upstream resolves each
//!   provider's own env chain / ADC / IAM files);
//! - `print-bearer-token` requires an OAuth credential (`auth.json`);
//!   `--min-expiry` enforces the requested remaining validity after the
//!   locked refresh (upstream `minOAuthValidityMs`);
//! - the `--model`-only scan matches stored credentials **plus** providers
//!   with a resolvable models.json `apiKey` (upstream scans stored only —
//!   pi-rs keeps its models.json credential layer usable);
//! - `--no-refresh` never touches the network: an expiring OAuth token
//!   reports `invalid`/`invalid_state` (upstream swaps in a read-only store,
//!   so the locked refresh cannot persist and the check fails the same way).
//!
//! Help/usage lines and error strings follow the upstream texts verbatim
//! with `APP_NAME` = `pi-rs`.

use crate::config::APP_NAME;
use crate::core::auth_storage::AuthContext;
use crate::core::model_registry::ModelRegistry;
use pi_ai::auth::ModelAuth;

/// Upstream `AuthCommandKind` (`auth-command.ts:5`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthCommandKind {
	Check,
	ApiKey,
	BearerToken,
}

impl AuthCommandKind {
	/// `getAuthCommandName` (`auth-command.ts:24-26`).
	fn name(self) -> &'static str {
		match self {
			AuthCommandKind::Check => "auth check",
			AuthCommandKind::ApiKey => "auth print-api-key",
			AuthCommandKind::BearerToken => "auth print-bearer-token",
		}
	}

	/// `getAuthCommandUsage` (`auth-command.ts:18-30`), `APP_NAME` swapped.
	fn usage(self) -> String {
		match self {
			AuthCommandKind::Check => format!(
				"{APP_NAME} auth check --provider <provider> [--json] [--credentials] [--no-refresh]"
			),
			AuthCommandKind::ApiKey => {
				format!("{APP_NAME} auth print-api-key --provider <provider> [--model <model>]")
			}
			AuthCommandKind::BearerToken => format!(
				"{APP_NAME} auth print-bearer-token --provider <provider> [--model <model>] [--min-expiry <duration>]"
			),
		}
	}
}

/// Parsed `auth` invocation — upstream `AuthCommand` plus the pieces its
/// caller re-derives through `parseArgs` (`unknownFlags`, `apiKey`,
/// positionals).
#[derive(Debug)]
struct AuthCommand {
	kind: AuthCommandKind,
	provider: Option<String>,
	model: Option<String>,
	api_key: Option<String>,
	positionals: usize,
	json: bool,
	credentials: bool,
	no_refresh: bool,
	min_expiry_ms: Option<u64>,
	unknown_flag: Option<String>,
}

/// `printAuthCommandHelp` (`auth-command.ts:39-46`) verbatim; the pi-rs
/// ambient-source note replaces upstream's (implicit) provider-env chain.
fn print_auth_command_help() {
	println!(
		"Usage:
  {APP_NAME} auth print-api-key [--provider <provider>] [--model <model>]
  {APP_NAME} auth print-bearer-token [--provider <provider>] [--model <model>] [--min-expiry <duration>]
  {APP_NAME} auth check [--provider <provider>] [--model <model>] [--json] [--credentials] [--no-refresh]

Auth commands require at least one of --provider or --model. Checks refresh expired OAuth credentials by default; --no-refresh prevents this. --credentials emits the credential, or includes it in JSON output.

pi-rs difference: ambient credentials resolve from models.json apiKey values ($ENV / !command included) and the adapter environment variables; OAuth tokens live in auth.json."
	);
}

/// Value of a flag at `index`: an inline `--flag=value`, else the next token
/// taken unconditionally when one exists (upstream checks only
/// `i + 1 < args.length`, so a `-`-leading token is still consumed). Returns
/// the value and the index of the last consumed token.
fn flag_value(args: &[String], index: usize, inline: Option<String>) -> (Option<String>, usize) {
	match inline {
		Some(value) => (Some(value), index),
		None => match args.get(index + 1) {
			Some(value) => (Some(value.clone()), index + 1),
			None => (None, index),
		},
	}
}

/// `parseAuthCommand` + the `parseArgs` pass its caller runs on the remaining
/// arguments (`auth-command.ts:48-96`, `main.ts:150-157`). Known flags are
/// collected; the first unknown flag is reported by name; a trailing
/// `--provider` with no value lands in `unknown_flag` like upstream (where
/// the value branch requires `i + 1 < args.length`).
fn parse_auth_command(args: &[String]) -> Result<AuthCommand, String> {
	let kind = match args[1].as_str() {
		"check" => AuthCommandKind::Check,
		"print-api-key" => AuthCommandKind::ApiKey,
		"print-bearer-token" => AuthCommandKind::BearerToken,
		other => {
			return Err(format!(
				"Unknown auth command \"{other}\". Use \"{APP_NAME} auth print-api-key\", \"{APP_NAME} auth print-bearer-token\", or \"{APP_NAME} auth check\"."
			));
		}
	};

	let mut command = AuthCommand {
		kind,
		provider: None,
		model: None,
		api_key: None,
		positionals: 0,
		json: false,
		credentials: false,
		no_refresh: false,
		min_expiry_ms: None,
		unknown_flag: None,
	};
	let mut index = 2;
	while index < args.len() {
		let arg = args[index].as_str();
		let (flag, inline_value) = match arg.strip_prefix("--").filter(|rest| !rest.is_empty()) {
			Some(rest) => match rest.split_once('=') {
				Some((name, value)) => (name.to_string(), Some(value.to_string())),
				None => (rest.to_string(), None),
			},
			None => (String::new(), None),
		};
		if flag == "min-expiry" {
			if kind != AuthCommandKind::BearerToken {
				return Err("--min-expiry is only supported by print-bearer-token".into());
			}
			let (value, consumed) = flag_value(args, index, inline_value);
			index = consumed;
			let Some(value) = value else {
				return Err("--min-expiry must use a duration such as 30m or 1h".into());
			};
			command.min_expiry_ms = Some(parse_min_expiry(&value)?);
		} else if flag == "json" || flag == "credentials" || flag == "no-refresh" {
			if kind != AuthCommandKind::Check {
				return Err(format!("{arg} is only supported by auth check"));
			}
			match flag.as_str() {
				"json" => command.json = true,
				"credentials" => command.credentials = true,
				_ => command.no_refresh = true,
			}
		} else if flag == "provider" || flag == "model" || flag == "api-key" {
			let (value, consumed) = flag_value(args, index, inline_value);
			index = consumed;
			match value {
				Some(value) => match flag.as_str() {
					"provider" => command.provider = Some(value),
					"model" => command.model = Some(value),
					_ => command.api_key = Some(value),
				},
				// No next token: upstream records the bare flag as unknown
				// (args.ts:255-260).
				None => command.unknown_flag = command.unknown_flag.or(Some(flag)),
			}
		} else if arg.starts_with('-') {
			// Unknown flag (keyed without the leading dashes, like
			// upstream's `unknownFlags`); a bare `-` counts as a positional.
			if arg == "-" {
				command.positionals += 1;
			} else {
				let key = arg.trim_start_matches('-');
				command.unknown_flag = command.unknown_flag.or(Some(key.to_string()));
			}
		} else {
			command.positionals += 1;
		}
		index += 1;
	}
	Ok(command)
}

/// `--min-expiry` duration grammar (`auth-command.ts:76-80`):
/// `^(\d+)(ms|s|m|h)$` case-insensitive.
fn parse_min_expiry(value: &str) -> Result<u64, String> {
	let lowered = value.to_lowercase();
	let digits = lowered.trim_end_matches(|c: char| c.is_ascii_alphabetic());
	let unit = &lowered[digits.len()..];
	let Ok(amount) = digits.parse::<u64>() else {
		return Err("--min-expiry must use a duration such as 30m or 1h".into());
	};
	let multiplier = match unit {
		"ms" => 1,
		"s" => 1_000,
		"m" => 60_000,
		"h" => 3_600_000,
		_ => return Err("--min-expiry must use a duration such as 30m or 1h".into()),
	};
	Ok(amount * multiplier)
}

/// `getAuthCredential` (`auth-command.ts:120-126`): the api key, or the
/// bearer token of an `authorization` header.
fn get_auth_credential(auth: &ModelAuth) -> Option<String> {
	if let Some(key) = &auth.api_key {
		return Some(key.clone());
	}
	auth.headers
		.iter()
		.find(|(name, _)| name.to_lowercase() == "authorization")
		.and_then(|(_, value)| {
			value
				.strip_prefix("Bearer ")
				.or_else(|| value.strip_prefix("bearer "))
				.map(str::to_string)
		})
}

/// Upstream `AuthCheckResult` (`auth-check.ts:15-20`) flattened to strings.
struct AuthCheckResult {
	status: String,
	provider: String,
	reason: Option<&'static str>,
	auth_type: Option<&'static str>,
}

/// Ambient credential sources in upstream's `envApiKeyAuth` shape: the
/// adapter env var each provider resolves at request time. Providers not
/// listed here have no ambient api-key source (OAuth-only or bespoke
/// credential chains like Bedrock/Vertex).
fn ambient_env_var(provider: &str) -> Option<&'static str> {
	match provider {
		"anthropic" => Some("ANTHROPIC_API_KEY"),
		"openai" => Some("OPENAI_API_KEY"),
		"mimo" => Some("MIMO_API_KEY"),
		"google-generative-ai" => Some("GEMINI_API_KEY"),
		"azure-openai-responses" => Some("AZURE_OPENAI_API_KEY"),
		"openrouter" => Some("OPENROUTER_API_KEY"),
		"xai" => Some("XAI_API_KEY"),
		"kimi-coding" => Some("KIMI_API_KEY"),
		_ => None,
	}
}

/// One resolved CLI credential (`getProviderCredential` /
/// `resolveCredentialForPrint` result) with its source label.
struct ResolvedCliAuth {
	value: String,
	auth_type: &'static str,
	source: String,
}

/// Resolve one provider's credential: auth.json stored credential first
/// (api-key resolved through `resolve_config_value`, OAuth through the
/// provider flow with the locked refresh unless `refresh` is false), then
/// the ambient chain (models.json `apiKey`, then the adapter env var).
/// `min_expiry_ms` (bearer export) requires the requested remaining
/// validity after the refresh.
async fn resolve_provider_credential(
	registry: &ModelRegistry,
	auth: &AuthContext,
	provider: &str,
	refresh: bool,
	min_expiry_ms: Option<u64>,
) -> Result<Option<ResolvedCliAuth>, String> {
	resolve_provider_credential_async(registry, auth, provider, refresh, min_expiry_ms).await
}

async fn resolve_provider_credential_async(
	registry: &ModelRegistry,
	auth: &AuthContext,
	provider: &str,
	refresh: bool,
	min_expiry_ms: Option<u64>,
) -> Result<Option<ResolvedCliAuth>, String> {
	// --no-refresh never touches the network: a stored OAuth token past the
	// five-minute window reports "cannot refresh" (upstream swaps in a
	// read-only store, whose modify() rejection fails the check the same
	// way); a fresh token resolves without I/O.
	let min_validity = min_expiry_ms.map(|ms| ms as i64);
	if !refresh
		&& let Some(pi_ai::auth::Credential::OAuth { expires, .. }) = auth.read_credential(provider)
	{
		let now = std::time::SystemTime::now()
			.duration_since(std::time::UNIX_EPOCH)
			.map(|d| d.as_millis() as i64)
			.unwrap_or(0);
		let minimum = pi_ai::auth::DEFAULT_OAUTH_MINIMUM_VALIDITY_MS.max(min_validity.unwrap_or(0));
		if now + minimum >= expires {
			return Err(
				"Stored OAuth token is expired and --no-refresh prevents a refresh".to_string(),
			);
		}
	}
	match auth.resolve_auth(provider, min_validity).await {
		Ok(Some((resolved, source))) => {
			let auth_type = if source == "OAuth" {
				"oauth"
			} else {
				"api_key"
			};
			Ok(get_auth_credential(&resolved).map(|value| ResolvedCliAuth {
				value,
				auth_type,
				source,
			}))
		}
		Ok(None) => {
			// Ambient: models.json apiKey, then the adapter env var.
			if let Some(key) = registry.resolve_api_key(provider) {
				return Ok(Some(ResolvedCliAuth {
					value: key,
					auth_type: "api_key",
					source: "models.json".to_string(),
				}));
			}
			if let Some(env_var) = ambient_env_var(provider)
				&& let Ok(value) = std::env::var(env_var)
				&& !value.is_empty()
			{
				return Ok(Some(ResolvedCliAuth {
					value,
					auth_type: "api_key",
					source: env_var.to_string(),
				}));
			}
			Ok(None)
		}
		Err(error) => Err(error),
	}
}

/// Entry point for `pi-rs auth <args>` — returns the process exit code.
/// Mirrors `runAuthCommand` (`main.ts:133-209`).
pub async fn run_auth_command(args: &[String]) -> i32 {
	// `isAuthCommandHelp` (auth-command.ts:32-37): bare `auth`, `auth help`,
	// or a `--help`/`-h` anywhere in the auth args.
	if args.len() == 1
		|| args.get(1).map(String::as_str) == Some("help")
		|| args.iter().any(|arg| arg == "--help" || arg == "-h")
	{
		print_auth_command_help();
		return 0;
	}

	let command = match parse_auth_command(args) {
		Ok(command) => command,
		Err(message) => {
			eprintln!("Error: {message}");
			return 1;
		}
	};

	// Unknown flags report before validation, with the dim usage hint
	// (main.ts:151-156), exit 1.
	if let Some(option) = &command.unknown_flag {
		eprintln!(
			"Error: Unknown option --{option} for \"{}\".",
			command.kind.name()
		);
		eprintln!("Use \"{APP_NAME} --help\" or \"{}\".", command.kind.usage());
		return 1;
	}

	let registry = ModelRegistry::load(None);
	let auth = AuthContext::shared();
	let error_exit = |message: &str| -> i32 {
		// main.ts:203-207 — check failures exit 2, print failures exit 1.
		eprintln!("Error: {message}");
		if command.kind == AuthCommandKind::Check {
			2
		} else {
			1
		}
	};

	// `validateAuthCommandArgs` (auth-command.ts:105-107): an `--api-key` or
	// a positional is rejected for every kind.
	if command.api_key.is_some() || command.positionals > 0 {
		return error_exit("Auth commands only accept --provider and --model");
	}
	let provider = trimmed(&command.provider);
	let model = trimmed(&command.model);

	match command.kind {
		AuthCommandKind::Check => {
			// `validateAuthCommandArgs` (auth-command.ts:108-112).
			if provider.is_none() && model.is_none() {
				return error_exit("Auth checks require --provider <provider> or --model <model>");
			}
			match check_provider_auth(
				&registry,
				auth,
				provider.as_deref(),
				model.as_deref(),
				!command.no_refresh,
			)
			.await
			{
				Err(message) => error_exit(&message),
				Ok(result) => {
					// `--credentials` attaches the resolved credential when
					// ready (main.ts:183-190 via getProviderCredential).
					let credential = if command.credentials && result.status == "ready" {
						resolve_provider_credential(
							&registry,
							auth,
							&result.provider,
							!command.no_refresh,
							None,
						)
						.await
						.ok()
						.flatten()
						.map(|resolved| resolved.value)
					} else {
						None
					};
					let output = if command.json {
						let mut value = serde_json::json!({
							"status": result.status,
							"provider": result.provider,
						});
						if let Some(reason) = result.reason {
							value["reason"] = serde_json::json!(reason);
						}
						if let Some(auth_type) = result.auth_type {
							value["authType"] = serde_json::json!(auth_type);
						}
						if let Some(credential) = credential {
							value["credentials"] = serde_json::json!(credential);
						}
						value.to_string()
					} else {
						credential.unwrap_or_else(|| result.status.clone())
					};
					println!("{output}");
					match result.status.as_str() {
						"ready" => 0,
						"not_ready" => 1,
						_ => 2,
					}
				}
			}
		}
		AuthCommandKind::ApiKey | AuthCommandKind::BearerToken => {
			// `validateAuthCommandArgs` (auth-command.ts:114-117).
			if provider.is_none() && model.is_none() {
				return error_exit(
					"Credential printing requires --provider <provider> or --model <model>",
				);
			}
			match resolve_credential_for_print(
				&registry,
				auth,
				command.kind,
				provider,
				model,
				command.min_expiry_ms,
			)
			.await
			{
				Ok(credential) => {
					println!("{credential}");
					0
				}
				Err(message) => error_exit(&message),
			}
		}
	}
}

/// `args.provider?.trim() || undefined` semantics: empty trims to nothing.
fn trimmed(value: &Option<String>) -> Option<String> {
	value
		.as_deref()
		.map(str::trim)
		.filter(|value| !value.is_empty())
		.map(str::to_owned)
}

/// `checkProviderAuth` (auth-check.ts:22-53): the provider must exist in the
/// catalog; a stored/ambient credential makes it `ready` with the credential
/// type, and with `refresh` enabled an expiring OAuth token must survive the
/// locked refresh (failures land in `invalid`/`invalid_state`, exit 2).
async fn check_provider_auth(
	registry: &ModelRegistry,
	auth: &AuthContext,
	provider: Option<&str>,
	model: Option<&str>,
	refresh: bool,
) -> Result<AuthCheckResult, String> {
	let mut provider = provider.map(str::to_owned);
	if let Some(model) = model {
		let resolved = match provider.as_deref() {
			Some(provider) => registry.find_for_provider(provider, model),
			None => registry.find(model),
		};
		let resolved = resolved.ok_or_else(|| format!("Unable to resolve model \"{model}\""))?;
		provider = Some(resolved.provider);
	}
	let Some(provider) = provider else {
		return Err("Unable to resolve an auth provider".into());
	};
	if !registry.has_provider(&provider) {
		return Ok(AuthCheckResult {
			status: "not_ready".into(),
			provider,
			reason: Some("provider_not_found"),
			auth_type: None,
		});
	}
	let resolved = resolve_provider_credential(registry, auth, &provider, refresh, None).await;
	let auth_type = match &resolved {
		Ok(Some(resolved)) => Some(resolved.auth_type),
		Ok(None) => None,
		// A refresh/resolution failure surfaces as invalid_state (upstream
		// catches the ModelsError and reports invalid), exit 2.
		Err(_) => {
			return Ok(AuthCheckResult {
				status: "invalid".into(),
				provider,
				reason: Some("invalid_state"),
				auth_type: None,
			});
		}
	};
	match auth_type {
		Some(auth_type) => Ok(AuthCheckResult {
			status: "ready".into(),
			provider,
			reason: None,
			auth_type: Some(auth_type),
		}),
		None => Ok(AuthCheckResult {
			status: "not_ready".into(),
			provider,
			reason: Some("credentials_not_configured"),
			auth_type: None,
		}),
	}
}

/// `resolveCredentialForPrint` (credential-print.ts:17-90). `api_key` skips
/// OAuth-type providers and vice versa, with upstream's exact mismatch
/// errors; the `--model`-only scan covers stored credentials plus providers
/// with a resolvable models.json key (pi-rs declared difference).
async fn resolve_credential_for_print(
	registry: &ModelRegistry,
	auth: &AuthContext,
	kind: AuthCommandKind,
	provider: Option<String>,
	model: Option<String>,
	min_expiry_ms: Option<u64>,
) -> Result<String, String> {
	let credential_type = |provider_id: &str| -> Option<&'static str> {
		match auth.read_credential(provider_id) {
			Some(credential) => match credential.type_label() {
				"oauth" => Some("oauth"),
				_ => Some("api_key"),
			},
			None => {
				if registry.resolve_api_key(provider_id).is_some()
					|| ambient_env_var(provider_id).is_some_and(|name| {
						std::env::var(name).is_ok_and(|value| !value.is_empty())
					}) {
					Some("api_key")
				} else {
					None
				}
			}
		}
	};

	let mut providers: Vec<(String, Option<String>)> = Vec::new();
	if let Some(provider) = &provider {
		if !registry.has_provider(provider) {
			return Err(format!(
				"Unknown provider \"{provider}\". Use --list-models to see available providers."
			));
		}
		if let Some(model) = &model
			&& registry.find_for_provider(provider, model).is_none()
		{
			return Err("Unable to resolve the requested provider/model".into());
		}
		providers.push((provider.clone(), model.clone()));
	} else {
		// `--model` only: scan stored credentials plus models.json-resolvable
		// providers (pi-rs declared difference; upstream scans stored only).
		let model = model.expect("validated: provider or model is required");
		let mut scanned: Vec<String> = auth
			.list_credentials()
			.into_iter()
			.map(|(id, _)| id)
			.collect();
		for candidate in registry.provider_ids() {
			if !scanned.contains(&candidate) {
				scanned.push(candidate);
			}
		}
		for candidate in scanned {
			if credential_type(&candidate).is_none() {
				continue;
			}
			if registry.find_for_provider(&candidate, &model).is_some() {
				providers.push((candidate, Some(model.clone())));
			}
		}
		if providers.is_empty() {
			return Err(format!(
				"Model \"{model}\" not found. Use --list-models to see available models."
			));
		}
	}

	let refresh = true;
	let mut matches: Vec<(String, String)> = Vec::new();
	for (provider_id, _model) in &providers {
		let credential_type = credential_type(provider_id);
		if kind == AuthCommandKind::ApiKey && credential_type == Some("oauth") {
			continue;
		}
		if kind == AuthCommandKind::BearerToken && credential_type != Some("oauth") {
			continue;
		}
		// Bearer export requires the requested minimum validity after the
		// refresh (upstream `minOAuthValidityMs`).
		let min_validity = match kind {
			AuthCommandKind::BearerToken => Some(min_expiry_ms.unwrap_or(30 * 60_000)),
			_ => None,
		};
		match resolve_provider_credential(registry, auth, provider_id, refresh, min_validity).await
		{
			Ok(Some(resolved)) => matches.push((provider_id.clone(), resolved.value)),
			Ok(None) => {}
			Err(error) => return Err(error),
		}
	}

	match matches.len() {
		1 => Ok(matches.remove(0).1),
		0 => {
			let provider_id = providers.first().map(|(id, _)| id.clone());
			let credential_type = provider_id.as_deref().and_then(credential_type);
			if provider.is_some()
				&& kind == AuthCommandKind::ApiKey
				&& credential_type == Some("oauth")
			{
				return Err(format!(
					"Provider \"{}\" is configured with OAuth, not an API key",
					provider_id.unwrap_or_default()
				));
			}
			if provider.is_some()
				&& kind == AuthCommandKind::BearerToken
				&& credential_type != Some("oauth")
			{
				return Err(format!(
					"Provider \"{}\" is not configured with an OAuth bearer token",
					provider_id.unwrap_or_default()
				));
			}
			Err(format!(
				"No usable {} is configured",
				if kind == AuthCommandKind::ApiKey {
					"API key"
				} else {
					"OAuth bearer token"
				}
			))
		}
		_ => Err(format!(
			"Multiple configured providers matched ({}). Specify --provider.",
			matches
				.iter()
				.map(|(provider, _)| provider.as_str())
				.collect::<Vec<_>>()
				.join(", ")
		)),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn args(list: &[&str]) -> Vec<String> {
		list.iter().map(|s| s.to_string()).collect()
	}

	#[test]
	fn min_expiry_parses_all_units() {
		assert_eq!(parse_min_expiry("30m").unwrap(), 1_800_000);
		assert_eq!(parse_min_expiry("1H").unwrap(), 3_600_000);
		assert_eq!(parse_min_expiry("500ms").unwrap(), 500);
		assert_eq!(parse_min_expiry("2s").unwrap(), 2_000);
		assert!(parse_min_expiry("30").is_err());
		assert!(parse_min_expiry("1x").is_err());
	}

	#[test]
	fn parse_rejects_flags_on_wrong_kinds() {
		assert!(parse_auth_command(&args(&["auth", "check", "--min-expiry", "1h"])).is_err());
		assert!(parse_auth_command(&args(&["auth", "print-api-key", "--json"])).is_err());
		assert!(parse_auth_command(&args(&["auth", "bogus"])).is_err());
		// A trailing bare flag lands in unknown_flag.
		let parsed = parse_auth_command(&args(&["auth", "check", "--provider"])).unwrap();
		assert_eq!(parsed.unknown_flag.as_deref(), Some("provider"));
	}

	#[test]
	fn get_auth_credential_reads_key_then_bearer_header() {
		let mut auth = ModelAuth::default();
		assert_eq!(get_auth_credential(&auth), None);
		auth.api_key = Some("sk-test".into());
		assert_eq!(get_auth_credential(&auth).as_deref(), Some("sk-test"));
		auth.api_key = None;
		auth.headers
			.insert("Authorization".into(), "Bearer abc.def".into());
		assert_eq!(get_auth_credential(&auth).as_deref(), Some("abc.def"));
	}

	#[test]
	fn ambient_env_var_covers_the_adapter_names() {
		assert_eq!(ambient_env_var("anthropic"), Some("ANTHROPIC_API_KEY"));
		assert_eq!(ambient_env_var("mimo"), Some("MIMO_API_KEY"));
		assert_eq!(ambient_env_var("openai-codex"), None, "OAuth-only upstream");
	}
}
