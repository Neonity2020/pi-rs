//! `pi-rs auth` subcommand — port of upstream `cli/auth-command.ts` +
//! `cli/auth-check.ts` + `cli/credential-print.ts`, cut to pi-rs's credential
//! model. Providers authenticate with models.json `apiKey` values (literal,
//! `$ENV` template, `!command` — [`ModelRegistry::resolve_api_key`]); pi-rs
//! has no OAuth credential store (`auth.json`) and no refresh path, so the
//! declared differences are:
//! - `auth check` reports `ready`/`not_ready` from key resolvability;
//!   `authType` is always `api_key` when ready (upstream also reports
//!   `oauth` and refreshes expired credentials unless `--no-refresh`);
//! - `--no-refresh` parses (with the upstream restriction to `auth check`)
//!   but has no effect;
//! - `auth print-bearer-token` always fails because no OAuth credential can
//!   exist — it uses upstream's own error text for that situation;
//! - `--min-expiry` parses with upstream's validation errors but has no
//!   effect.
//!
//! Help/usage lines and error strings follow the upstream texts verbatim
//! with `APP_NAME` = `pi-rs`.

use crate::config::APP_NAME;
use crate::core::model_registry::ModelRegistry;

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
/// positionals). `min_expiry_ms` parses for validation parity but has no
/// effect in this port (no OAuth tokens to expire).
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

/// `printAuthCommandHelp` (`auth-command.ts:39-46`) with the OAuth sentence
/// kept verbatim and the pi-rs credential cut appended as a declared
/// difference.
fn print_auth_command_help() {
	println!(
		"Usage:
  {APP_NAME} auth print-api-key [--provider <provider>] [--model <model>]
  {APP_NAME} auth print-bearer-token [--provider <provider>] [--model <model>] [--min-expiry <duration>]
  {APP_NAME} auth check [--provider <provider>] [--model <model>] [--json] [--credentials] [--no-refresh]

Auth commands require at least one of --provider or --model. Checks refresh expired OAuth credentials by default; --no-refresh prevents this. --credentials emits the credential, or includes it in JSON output.

pi-rs difference: there is no OAuth credential store — credentials resolve from models.json apiKey values ($ENV / !command included), so nothing is refreshed and bearer tokens are never available."
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

/// Entry point for `pi-rs auth <args>` — returns the process exit code.
/// Mirrors `runAuthCommand` (`main.ts:133-209`).
pub fn run_auth_command(args: &[String]) -> i32 {
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
			match check_provider_auth(&registry, provider.as_deref(), model.as_deref()) {
				Err(message) => error_exit(&message),
				Ok(result) => {
					// `--credentials` attaches the key when ready
					// (main.ts:183-190); there is no OAuth store to refresh.
					let credential = if command.credentials && result.status == "ready" {
						registry.resolve_api_key(&result.provider)
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
			match resolve_credential_for_print(&registry, command.kind, provider, model) {
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

/// Upstream `AuthCheckResult` (`auth-check.ts:15-20`) flattened to strings;
/// `invalid` is unreachable in this port (no runtime/model-network state at
/// check time) but stays in the exit-code mapping for parity.
struct AuthCheckResult {
	status: String,
	provider: String,
	reason: Option<&'static str>,
	auth_type: Option<&'static str>,
}

/// `checkProviderAuth` (auth-check.ts:22-53) reduced to the pi-rs credential
/// model: a provider is `ready` when its models.json `apiKey` resolves,
/// `not_ready` (`provider_not_found`) when the catalog has no such provider,
/// and `not_ready` (`credentials_not_configured`) when the key is absent or
/// fails to resolve. `Err` carries the `AuthCommandError` texts (exit 2).
fn check_provider_auth(
	registry: &ModelRegistry,
	provider: Option<&str>,
	model: Option<&str>,
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
	match registry.resolve_api_key(&provider) {
		Some(_) => Ok(AuthCheckResult {
			status: "ready".into(),
			provider,
			reason: None,
			auth_type: Some("api_key"),
		}),
		None => Ok(AuthCheckResult {
			status: "not_ready".into(),
			provider,
			reason: Some("credentials_not_configured"),
			auth_type: None,
		}),
	}
}

/// `resolveCredentialForPrint` (credential-print.ts:17-90) cut to API keys.
/// `BearerToken` always fails: pi-rs has no OAuth credentials, so the code
/// paths collapse to upstream's own failure texts for "provider exists but is
/// not OAuth" and "no OAuth provider can match a model query".
fn resolve_credential_for_print(
	registry: &ModelRegistry,
	kind: AuthCommandKind,
	provider: Option<String>,
	model: Option<String>,
) -> Result<String, String> {
	if let Some(provider) = provider {
		if !registry.has_provider(&provider) {
			return Err(format!(
				"Unknown provider \"{provider}\". Use --list-models to see available providers."
			));
		}
		if kind == AuthCommandKind::BearerToken {
			return Err(format!(
				"Provider \"{provider}\" is not configured with an OAuth bearer token"
			));
		}
		if let Some(model) = &model
			&& registry.find_for_provider(&provider, model).is_none()
		{
			return Err("Unable to resolve the requested provider/model".into());
		}
		return registry
			.resolve_api_key(&provider)
			.ok_or_else(|| "No usable API key is configured".to_string());
	}

	// `--model` only: scan every provider for one that both holds a usable
	// credential and resolves the query (credential-print.ts:43-57).
	let model = model.expect("validated: provider or model is required");
	if kind == AuthCommandKind::BearerToken {
		// Upstream filters the scan to OAuth credentials — an empty set here
		// — and reports the miss with its model-not-found text.
		return Err(format!(
			"Model \"{model}\" not found. Use --list-models to see available models."
		));
	}
	let mut matches: Vec<(String, String)> = Vec::new();
	for candidate in registry.provider_ids() {
		let Some(key) = registry.resolve_api_key(&candidate) else {
			continue;
		};
		if registry.find_for_provider(&candidate, &model).is_some() {
			matches.push((candidate, key));
		}
	}
	match matches.len() {
		0 => Err(format!(
			"Model \"{model}\" not found. Use --list-models to see available models."
		)),
		1 => Ok(matches.remove(0).1),
		_ => Err(format!(
			"Multiple configured providers matched ({}). Specify --provider.",
			matches
				.into_iter()
				.map(|(provider, _)| provider)
				.collect::<Vec<_>>()
				.join(", ")
		)),
	}
}
