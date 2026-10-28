//! Model registry — port of
//! `pi/packages/coding-agent/src/core/model-registry.ts` + `provider-composer.ts`.
//!
//! Framework stage: a small built-in catalog plus `~/.pi-rs/agent/models.json`
//! custom entries. The file format matches upstream
//! (`docs/models-configuration.md`): `providers` is an **object** keyed by
//! provider id, each provider carries `name?/baseUrl?/apiKey?/api?/headers?/
//! models?/modelOverrides?`, and each model entry supports
//! `id/name/api/baseUrl/reasoning/input/cost/contextWindow/maxTokens/headers`
//! with upstream defaults (`contextWindow` 128000, `maxTokens` **16384**).
//! `oauth`/`compat`/`authHeader`/`thinkingLevelMap`/`inputLimits`/
//! `samplingParams`/`promptCache` are tolerated as unrecognized fields until
//! the provider adapters need them.
//!
//! `apiKey` and `headers` values resolve like upstream
//! `resolve-config-value.ts`: `!command` runs a shell command (stdout,
//! trimmed, 10s cap, cached for the process lifetime), `$VAR`/`${VAR}`
//! interpolate environment variables (an empty value counts as missing and
//! makes the whole value unresolved), `$$`/`$!` escape literal `$`/`!`.
//!
//! Backward compatibility (transitional): the previous pi-rs array format
//! (`providers` as an array of `{name, api?, baseUrl?, models?}`) is still
//! read when `providers` is a JSON array, with the old defaults
//! (`maxTokens` 8192, no key resolution). Detection is by JSON shape.
//!
//! Not ported (P2+): the generated catalog hydration from pi's official
//! models.json, virtual models, OAuth providers, and the auth-status
//! composer.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::Value;

use pi_ai::{Api, Model, ModelCost};

use crate::config;

pub struct ModelRegistry {
	models: Vec<Model>,
	/// Raw `apiKey`/`headers` config per provider id (models.json object
	/// format), as written. Values go through [`resolve_config_value`] at
	/// request-build time; request plumbing consumes them later.
	provider_auth: BTreeMap<String, ProviderAuth>,
}

/// Raw provider auth configuration from models.json.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProviderAuth {
	pub api_key: Option<String>,
	pub headers: BTreeMap<String, String>,
}

impl ModelRegistry {
	/// Load the catalog: built-ins plus `~/.pi-rs/agent/models.json`.
	pub fn load(settings_default_model: Option<&str>) -> Self {
		let mut models = builtin_models();
		let mut provider_auth = BTreeMap::new();
		if let Ok(text) = std::fs::read_to_string(config::models_json_path()) {
			provider_auth = apply_models_json(&mut models, &text);
		}

		// Explicit settings take priority; otherwise use the MiMo Token Plan.
		let default_model = settings_default_model
			.map(|s| s.to_string())
			.or_else(|| Some("mimo/mimo-v2.6-pro".to_string()));
		if let Some(default_key) = default_model
			&& let Some(model) = models.iter_mut().find(|m| matches_key(m, &default_key))
		{
			model.name = format!("{} (default)", model.name);
		}

		Self {
			models,
			provider_auth,
		}
	}

	pub fn models(&self) -> &[Model] {
		&self.models
	}

	/// Raw per-provider auth config from models.json (empty when the file is
	/// absent, legacy-shaped, or the provider carries no `apiKey`/`headers`).
	pub fn provider_auth(&self, provider: &str) -> Option<&ProviderAuth> {
		self.provider_auth.get(provider)
	}

	pub fn default_model(&self) -> Model {
		if let Some(model) = self.models.iter().find(|m| m.name.ends_with("(default)")) {
			return model.clone();
		}
		self.find("mimo/mimo-v2.6-pro")
			.expect("registry always contains the MiMo model")
	}

	/// Find by exact key (`provider/id`), id, or case-insensitive substring.
	pub fn find(&self, query: &str) -> Option<Model> {
		let query = query.trim();
		self.models
			.iter()
			.find(|m| matches_key(m, query))
			.or_else(|| self.models.iter().find(|m| m.id == query))
			.or_else(|| {
				self.models.iter().find(|m| {
					m.key().to_lowercase().contains(&query.to_lowercase())
						|| m.name.to_lowercase().contains(&query.to_lowercase())
				})
			})
			.cloned()
	}

	pub fn set_default(&mut self, model: &Model) {
		for m in &mut self.models {
			if m.name.ends_with("(default)") {
				m.name = m.name.replace(" (default)", "");
			}
		}
		if let Some(m) = self
			.models
			.iter_mut()
			.find(|m| m.id == model.id && m.provider == model.provider)
		{
			m.name = format!("{} (default)", m.name);
		}
	}
}

fn matches_key(model: &Model, key: &str) -> bool {
	let key = key.replace(" (default)", "");
	model.key() == key || (model.id == key) || format!("{}/{}", model.provider, model.id) == key
}

// -------------------------------------------------------------------------------------------
// models.json parsing (upstream `model-registry.ts` / docs/models-configuration.md)
// -------------------------------------------------------------------------------------------

/// Upstream defaults for model entries.
const DEFAULT_CONTEXT_WINDOW: u64 = 128_000;
const DEFAULT_MAX_TOKENS: u64 = 16_384;
/// Legacy pi-rs default (pre object-format); kept for the transitional
/// array path only.
const LEGACY_MAX_TOKENS: u64 = 8_192;

/// Parse `text` as models.json and apply it onto `models` (built-ins first).
/// Returns the provider auth config (apiKey/headers) declared by the file.
///
/// An unparseable file leaves the catalog untouched, like the previous
/// behavior. A `providers` array selects the legacy pi-rs path.
pub fn apply_models_json(models: &mut Vec<Model>, text: &str) -> BTreeMap<String, ProviderAuth> {
	let mut provider_auth = BTreeMap::new();
	let Ok(file) = serde_json::from_str::<ModelsFile>(text) else {
		return provider_auth;
	};
	match file.providers {
		Value::Object(providers) => apply_providers_object(models, &mut provider_auth, &providers),
		Value::Array(_) => {
			if let Ok(legacy) = serde_json::from_value::<Vec<LegacyProvider>>(file.providers) {
				apply_legacy_providers(models, legacy);
			}
		}
		_ => {}
	}
	provider_auth
}

#[derive(Deserialize)]
struct ModelsFile {
	#[serde(default)]
	providers: Value,
}

/// One provider entry in the upstream object format. Unrecognized fields
/// (oauth, compat, authHeader, ...) are tolerated and ignored.
#[derive(Debug, Deserialize, Clone, Default)]
#[serde(rename_all = "camelCase", default)]
struct ProviderEntry {
	name: Option<String>,
	base_url: Option<String>,
	api_key: Option<String>,
	api: Option<String>,
	headers: Option<BTreeMap<String, String>>,
	models: Vec<Value>,
	model_overrides: BTreeMap<String, Value>,
}

/// One model entry in the upstream object format. `apiKey`/`headers` are
/// parsed for forward compatibility but unused until request plumbing
/// consumes them (upstream models.json has no model-level apiKey).
#[derive(Debug, Deserialize, Clone, Default)]
#[serde(rename_all = "camelCase", default)]
struct CustomModel {
	id: String,
	name: Option<String>,
	api: Option<String>,
	base_url: Option<String>,
	api_key: Option<String>,
	reasoning: Option<bool>,
	input: Option<Vec<pi_ai::InputModality>>,
	cost: Option<PartialCost>,
	context_window: Option<u64>,
	max_tokens: Option<u64>,
	headers: Option<BTreeMap<String, String>>,
}

/// Cost fields that merge field-by-field (upstream `modelOverrides` rule).
#[derive(Debug, Deserialize, Clone, Copy, Default)]
#[serde(rename_all = "camelCase", default)]
struct PartialCost {
	input: Option<f64>,
	output: Option<f64>,
	cache_read: Option<f64>,
	cache_write: Option<f64>,
}

impl PartialCost {
	fn to_cost(self) -> ModelCost {
		ModelCost {
			input: self.input.unwrap_or(0.0),
			output: self.output.unwrap_or(0.0),
			cache_read: self.cache_read,
			cache_write: self.cache_write,
		}
	}

	fn merge_into(self, cost: &mut ModelCost) {
		if let Some(input) = self.input {
			cost.input = input;
		}
		if let Some(output) = self.output {
			cost.output = output;
		}
		if self.cache_read.is_some() {
			cost.cache_read = self.cache_read;
		}
		if self.cache_write.is_some() {
			cost.cache_write = self.cache_write;
		}
	}
}

/// `modelOverrides` entry: object fields replace (cost merges field by
/// field); fields left out leave the model untouched.
#[derive(Debug, Deserialize, Clone, Default)]
#[serde(rename_all = "camelCase", default)]
struct ModelOverride {
	name: Option<String>,
	reasoning: Option<bool>,
	input: Option<Vec<pi_ai::InputModality>>,
	cost: Option<PartialCost>,
	context_window: Option<u64>,
	max_tokens: Option<u64>,
	headers: Option<BTreeMap<String, String>>,
}

fn apply_providers_object(
	models: &mut Vec<Model>,
	provider_auth: &mut BTreeMap<String, ProviderAuth>,
	providers: &serde_json::Map<String, Value>,
) {
	for (provider_id, entry_value) in providers {
		let Ok(entry) = serde_json::from_value::<ProviderEntry>(entry_value.clone()) else {
			continue;
		};
		if entry.api_key.is_some() || entry.headers.is_some() {
			provider_auth.insert(
				provider_id.clone(),
				ProviderAuth {
					api_key: entry.api_key.clone(),
					headers: entry.headers.clone().unwrap_or_default(),
				},
			);
		}

		// A built-in provider id changes that provider: baseUrl/api also
		// apply to its built-in models.
		let builtin = models.iter().any(|m| m.provider == *provider_id);
		if builtin {
			for model in models.iter_mut().filter(|m| m.provider == *provider_id) {
				if let Some(base_url) = &entry.base_url {
					model.base_url = base_url.clone();
				}
				if let Some(api) = &entry.api {
					model.api = Api::new(api.clone());
				}
			}
		}

		// Each models entry adds a model; an entry whose id matches an
		// existing model of this provider replaces it. Invalid entries are
		// skipped (upstream reports and continues).
		for model_value in &entry.models {
			let Ok(model) = serde_json::from_value::<CustomModel>(model_value.clone()) else {
				continue;
			};
			if model.id.is_empty() {
				continue;
			}
			let api = Api::new(
				model
					.api
					.or_else(|| entry.api.clone())
					.unwrap_or_else(|| Api::OPENAI_COMPLETIONS.to_string()),
			);
			let base_url = model
				.base_url
				.or_else(|| entry.base_url.clone())
				.unwrap_or_default();
			let built = Model {
				id: model.id.clone(),
				name: model.name.unwrap_or_else(|| model.id.clone()),
				api,
				provider: provider_id.clone(),
				base_url,
				reasoning: model.reasoning.unwrap_or(false),
				input: model
					.input
					.unwrap_or_else(|| vec![pi_ai::InputModality::Text]),
				cost: model.cost.map(PartialCost::to_cost).unwrap_or_default(),
				context_window: model.context_window.unwrap_or(DEFAULT_CONTEXT_WINDOW),
				max_tokens: model.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
			};
			match models
				.iter_mut()
				.find(|m| m.provider == *provider_id && m.id == model.id)
			{
				Some(existing) => *existing = built,
				None => models.push(built),
			}
		}

		// modelOverrides apply to built-in or custom models by id; unknown
		// ids are ignored (upstream rule).
		for (model_id, override_value) in &entry.model_overrides {
			let Ok(over) = serde_json::from_value::<ModelOverride>(override_value.clone()) else {
				continue;
			};
			if let Some(model) = models.iter_mut().find(|m| &m.id == model_id) {
				if let Some(name) = over.name {
					model.name = name;
				}
				if let Some(reasoning) = over.reasoning {
					model.reasoning = reasoning;
				}
				if let Some(input) = over.input {
					model.input = input;
				}
				if let Some(cost) = over.cost {
					cost.merge_into(&mut model.cost);
				}
				if let Some(context_window) = over.context_window {
					model.context_window = context_window;
				}
				if let Some(max_tokens) = over.max_tokens {
					model.max_tokens = max_tokens;
				}
			}
		}
	}
}

// -------------------------------------------------------------------------------------------
// Legacy pi-rs array format (transitional compatibility)
// -------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct LegacyProvider {
	name: String,
	#[serde(default)]
	api: Option<String>,
	#[serde(default)]
	base_url: Option<String>,
	#[serde(default)]
	models: Vec<LegacyModel>,
}

#[derive(Debug, Deserialize)]
struct LegacyModel {
	id: String,
	#[serde(default)]
	name: Option<String>,
	#[serde(default)]
	context_window: Option<u64>,
	#[serde(default)]
	max_tokens: Option<u64>,
}

fn apply_legacy_providers(models: &mut Vec<Model>, providers: Vec<LegacyProvider>) {
	for provider in providers {
		let api = Api::new(
			provider
				.api
				.unwrap_or_else(|| Api::OPENAI_COMPLETIONS.to_string()),
		);
		for model in provider.models {
			models.push(Model {
				id: model.id.clone(),
				name: model.name.unwrap_or_else(|| model.id.clone()),
				api: api.clone(),
				provider: provider.name.clone(),
				base_url: provider.base_url.clone().unwrap_or_default(),
				reasoning: false,
				input: vec![pi_ai::InputModality::Text],
				cost: Default::default(),
				context_window: model.context_window.unwrap_or(DEFAULT_CONTEXT_WINDOW),
				max_tokens: model.max_tokens.unwrap_or(LEGACY_MAX_TOKENS),
			});
		}
	}
}

// -------------------------------------------------------------------------------------------
// Config value resolution — port of `core/resolve-config-value.ts`
// -------------------------------------------------------------------------------------------

/// Process-lifetime cache for `!command` results (upstream
/// `commandResultCache`).
fn command_result_cache() -> &'static std::sync::Mutex<HashMap<String, Option<String>>> {
	static CACHE: std::sync::OnceLock<std::sync::Mutex<HashMap<String, Option<String>>>> =
		std::sync::OnceLock::new();
	CACHE.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

/// Resolve a config value (API key, header value, ...) that may be a shell
/// command or contain environment variable references.
///
/// - `!command`: execute the rest via `sh -c`, use the trimmed stdout
///   (cached per command for the process lifetime; failures resolve to
///   `None`).
/// - `$VAR` / `${VAR}`: interpolate the named environment variable; a
///   missing or empty variable makes the whole value unresolved.
/// - `$$` and `$!` escape literal `$` and `!` in non-command values.
/// - Anything else is a literal.
///
/// `env` overrides the process environment (upstream passes an explicit map
/// for tests and request-time resolution).
pub fn resolve_config_value(
	config: &str,
	env: Option<&BTreeMap<String, String>>,
) -> Option<String> {
	match config.strip_prefix('!') {
		Some(command) => execute_config_command(command),
		None => resolve_config_template(config, env),
	}
}

/// Resolve all header values with the same logic as API keys; entries that
/// fail to resolve are dropped (upstream `resolveHeaders`).
pub fn resolve_headers(
	headers: &BTreeMap<String, String>,
	env: Option<&BTreeMap<String, String>>,
) -> BTreeMap<String, String> {
	headers
		.iter()
		.filter_map(|(key, value)| {
			resolve_config_value(value, env).map(|resolved| (key.clone(), resolved))
		})
		.collect()
}

fn resolve_env_var(name: &str, env: Option<&BTreeMap<String, String>>) -> Option<String> {
	// Upstream treats an empty value as missing (`env?.[name] || process.env[name]`).
	match env {
		Some(map) => map.get(name).filter(|value| !value.is_empty()).cloned(),
		None => std::env::var(name).ok().filter(|value| !value.is_empty()),
	}
}

fn is_env_name_start(c: char) -> bool {
	c.is_ascii_alphabetic() || c == '_'
}

fn is_env_name_char(c: char) -> bool {
	c.is_ascii_alphanumeric() || c == '_'
}

fn is_env_name(name: &str) -> bool {
	let mut chars = name.chars();
	match chars.next() {
		Some(first) if is_env_name_start(first) => chars.all(is_env_name_char),
		_ => false,
	}
}

/// `parseConfigValueTemplate` + `resolveTemplate`: walk the value, expanding
/// `$` references; any missing variable makes the whole value unresolved.
fn resolve_config_template(config: &str, env: Option<&BTreeMap<String, String>>) -> Option<String> {
	let mut resolved = String::new();
	let mut rest = config;
	while let Some(dollar) = rest.find('$') {
		resolved.push_str(&rest[..dollar]);
		let after = &rest[dollar + 1..];
		let mut chars = after.chars();
		match chars.next() {
			// `$$` and `$!` escape a literal `$` / `!`.
			Some(escaped @ ('$' | '!')) => {
				resolved.push(escaped);
				rest = &after[1..];
			}
			Some('{') => match after.find('}') {
				Some(end) => {
					// `after` starts at the brace; the name sits between it
					// and the closing brace.
					let name = &after[1..end];
					if is_env_name(name) {
						resolved.push_str(&resolve_env_var(name, env)?);
					} else {
						// Invalid name: the whole `${...}` including the `$`
						// is literal (upstream slices from dollarIndex).
						resolved.push('$');
						resolved.push_str(&after[..=end]);
					}
					rest = &after[end + 1..];
				}
				None => {
					// No closing brace: a literal `$`, rescan after it.
					resolved.push('$');
					rest = after;
				}
			},
			Some(first) if is_env_name_start(first) => {
				let name: String = after.chars().take_while(|c| is_env_name_char(*c)).collect();
				resolved.push_str(&resolve_env_var(&name, env)?);
				rest = &after[name.len()..];
			}
			_ => {
				resolved.push('$');
				rest = after;
			}
		}
	}
	resolved.push_str(rest);
	Some(resolved)
}

fn execute_config_command(command: &str) -> Option<String> {
	if let Some(hit) = command_result_cache().lock().unwrap().get(command) {
		return hit.clone();
	}
	let result = execute_config_command_uncached(command);
	command_result_cache()
		.lock()
		.unwrap()
		.insert(command.to_string(), result.clone());
	result
}

/// Upstream caps `!command` execution at 10 seconds and uses the trimmed
/// stdout; a non-zero exit or spawn failure resolves to `None`.
fn execute_config_command_uncached(command: &str) -> Option<String> {
	#[cfg(unix)]
	{
		use std::io::Read;
		use std::process::{Command, Stdio};
		let mut child = Command::new("sh")
			.arg("-c")
			.arg(command)
			.stdin(Stdio::null())
			.stdout(Stdio::piped())
			.stderr(Stdio::null())
			.spawn()
			.ok()?;
		let mut pipe = child.stdout.take()?;
		// Drain stdout on a thread so a chatty command cannot deadlock on a
		// full pipe while we wait for the exit status.
		let reader = std::thread::spawn(move || {
			let mut buffer = String::new();
			let _ = pipe.read_to_string(&mut buffer);
			buffer
		});
		let deadline = Instant::now() + Duration::from_secs(10);
		let status = loop {
			match child.try_wait() {
				Ok(Some(status)) => break Some(status),
				Ok(None) if Instant::now() >= deadline => break None,
				Ok(None) => std::thread::sleep(Duration::from_millis(25)),
				Err(_) => return None,
			}
		};
		if status.is_none() {
			let _ = child.kill();
			let _ = child.wait();
			return None;
		}
		let status = status?;
		if !status.success() {
			return None;
		}
		let output = reader.join().ok()?;
		let trimmed = output.trim();
		if trimmed.is_empty() {
			None
		} else {
			Some(trimmed.to_string())
		}
	}
	#[cfg(not(unix))]
	{
		let _ = command;
		None
	}
}

fn builtin_models() -> Vec<Model> {
	let mimo = Model::new(
		"mimo-v2.6-pro",
		"MiMo V2.6 Pro (Token Plan CN)",
		Api::OPENAI_COMPLETIONS,
		"mimo",
		std::env::var("MIMO_BASE_URL")
			.unwrap_or_else(|_| "https://token-plan-cn.xiaomimimo.com/v1".to_string()),
	);
	let mut mock = Model::new("mock-echo", "Mock (built-in)", Api::new("mock"), "mock", "");
	mock.context_window = 128_000;
	mock.max_tokens = 8_192;

	let sonnet = Model::new(
		"claude-sonnet-4-6",
		"Claude Sonnet 4.6",
		Api::ANTHROPIC_MESSAGES,
		"anthropic",
		"https://api.anthropic.com",
	);
	let mut sonnet = sonnet;
	sonnet.reasoning = true;
	sonnet.context_window = 200_000;
	sonnet.max_tokens = 64_000;
	sonnet.cost = pi_ai::ModelCost {
		input: 3.0,
		output: 15.0,
		..Default::default()
	};

	let gpt = Model::new(
		"gpt-5.2",
		"GPT-5.2",
		Api::OPENAI_COMPLETIONS,
		"openai",
		"https://api.openai.com/v1",
	);
	let mut gpt = gpt;
	gpt.reasoning = true;
	gpt.context_window = 400_000;
	gpt.max_tokens = 128_000;

	vec![mimo, mock, sonnet, gpt]
}

#[cfg(test)]
mod tests {
	use super::*;

	fn apply(text: &str) -> (Vec<Model>, BTreeMap<String, ProviderAuth>) {
		let mut models = builtin_models();
		let auth = apply_models_json(&mut models, text);
		(models, auth)
	}

	fn find<'a>(models: &'a [Model], provider: &str, id: &str) -> &'a Model {
		models
			.iter()
			.find(|m| m.provider == provider && m.id == id)
			.unwrap_or_else(|| panic!("missing model {provider}/{id}"))
	}

	#[test]
	fn object_format_builds_models_with_upstream_defaults() {
		let (models, _) = apply(
			r#"{
				"providers": {
					"acme": {
						"name": "Acme",
						"baseUrl": "https://api.acme.dev/v1",
						"api": "openai-completions",
						"models": [
							{ "id": "acme-large" },
							{
								"id": "acme-mini",
								"name": "Acme Mini",
								"reasoning": true,
								"input": ["text", "image"],
								"contextWindow": 400000,
								"maxTokens": 32000,
								"cost": { "input": 1.5, "output": 6.0, "cacheRead": 0.15 }
							}
						]
					}
				}
			}"#,
		);
		let large = find(&models, "acme", "acme-large");
		assert_eq!(large.name, "acme-large", "name defaults to the id");
		assert_eq!(large.api, Api::new("openai-completions"));
		assert_eq!(large.base_url, "https://api.acme.dev/v1");
		assert!(!large.reasoning);
		assert_eq!(large.input, vec![pi_ai::InputModality::Text]);
		assert_eq!(large.context_window, DEFAULT_CONTEXT_WINDOW);
		assert_eq!(large.max_tokens, 16_384, "upstream default maxTokens");

		let mini = find(&models, "acme", "acme-mini");
		assert_eq!(mini.name, "Acme Mini");
		assert!(mini.reasoning);
		assert_eq!(
			mini.input,
			vec![pi_ai::InputModality::Text, pi_ai::InputModality::Image]
		);
		assert_eq!(mini.context_window, 400_000);
		assert_eq!(mini.max_tokens, 32_000);
		assert_eq!(mini.cost.input, 1.5);
		assert_eq!(mini.cost.output, 6.0);
		assert_eq!(mini.cost.cache_read, Some(0.15));
	}

	#[test]
	fn model_level_fields_override_provider_level() {
		let (models, _) = apply(
			r#"{
				"providers": {
					"p": {
						"baseUrl": "https://provider.example",
						"api": "anthropic-messages",
						"models": [
							{ "id": "m", "api": "openai-responses", "baseUrl": "https://model.example" }
						]
					}
				}
			}"#,
		);
		let model = find(&models, "p", "m");
		assert_eq!(model.api, Api::new("openai-responses"));
		assert_eq!(model.base_url, "https://model.example");
	}

	#[test]
	fn entry_matching_existing_model_replaces_it() {
		let (models, _) = apply(
			r#"{
				"providers": {
					"anthropic": {
						"models": [
							{ "id": "claude-sonnet-4-6", "name": "Sonnet (custom)", "contextWindow": 1000, "maxTokens": 100 }
						]
					}
				}
			}"#,
		);
		let sonnet = find(&models, "anthropic", "claude-sonnet-4-6");
		assert_eq!(sonnet.name, "Sonnet (custom)");
		assert_eq!(sonnet.context_window, 1000);
		// The entry replaced the built-in wholesale (upstream "replaces it").
		assert!(!sonnet.reasoning);
		// Provider-level baseUrl/api would still have applied before the
		// replacement; none given here.
	}

	#[test]
	fn builtin_provider_id_changes_that_provider() {
		let (models, auth) = apply(
			r#"{
				"providers": {
					"anthropic": {
						"baseUrl": "https://anthropic-proxy.example",
						"apiKey": "$ACME_KEY",
						"headers": { "X-Scope": "team" }
					}
				}
			}"#,
		);
		let sonnet = find(&models, "anthropic", "claude-sonnet-4-6");
		assert_eq!(sonnet.base_url, "https://anthropic-proxy.example");
		assert_eq!(
			auth.get("anthropic").unwrap().api_key.as_deref(),
			Some("$ACME_KEY")
		);
		assert_eq!(
			auth.get("anthropic")
				.unwrap()
				.headers
				.get("X-Scope")
				.map(String::as_str),
			Some("team")
		);
	}

	#[test]
	fn model_overrides_patch_builtins_and_ignore_unknown_ids() {
		let (models, _) = apply(
			r#"{
				"providers": {
					"overrides": {
						"modelOverrides": {
							"gpt-5.2": {
								"name": "GPT (overridden)",
								"contextWindow": 900000,
								"maxTokens": 96000,
								"cost": { "input": 2.0 }
							},
							"no-such-model": { "name": "ghost" }
						}
					}
				}
			}"#,
		);
		let gpt = find(&models, "openai", "gpt-5.2");
		assert_eq!(gpt.name, "GPT (overridden)");
		assert_eq!(gpt.context_window, 900_000);
		assert_eq!(gpt.max_tokens, 96_000);
		// cost merges field-by-field: output stays at the built-in value.
		assert_eq!(gpt.cost.input, 2.0);
		assert_eq!(gpt.cost.output, 0.0);
		// reasoning was not overridden.
		assert!(gpt.reasoning);
	}

	#[test]
	fn legacy_array_format_still_loads() {
		let (models, auth) = apply(
			r#"{
				"providers": [
					{
						"name": "legacy-provider",
						"api": "openai-completions",
						"baseUrl": "https://legacy.example/v1",
						"models": [{ "id": "legacy-model", "name": "Legacy" }]
					}
				]
			}"#,
		);
		assert!(auth.is_empty(), "legacy path carries no auth config");
		let model = find(&models, "legacy-provider", "legacy-model");
		assert_eq!(model.name, "Legacy");
		assert_eq!(model.context_window, 128_000);
		assert_eq!(
			model.max_tokens, 8_192,
			"legacy default kept for transition"
		);
	}

	#[test]
	fn invalid_file_leaves_catalog_untouched() {
		let (models, auth) = apply("{ not json at all");
		assert_eq!(models.len(), builtin_models().len());
		assert!(auth.is_empty());
	}

	#[test]
	fn unknown_fields_are_tolerated() {
		let (models, _) = apply(
			r#"{
				"providers": {
					"p": {
						"oauth": "radius",
						"compat": { "thinkingFormat": "openai" },
						"authHeader": true,
						"models": [
							{ "id": "m", "thinkingLevelMap": { "high": "hi" }, "samplingParams": { "t": 1 }, "compat": {} }
						],
						"modelOverrides": { "m": { "promptCache": { "short": 300 } } }
					}
				}
			}"#,
		);
		let model = find(&models, "p", "m");
		assert_eq!(model.id, "m");
	}

	#[test]
	fn env_expansion_forms() {
		let mut env = BTreeMap::new();
		env.insert("PI_RS_TEST_KEY".to_string(), "secret-value".to_string());

		assert_eq!(
			resolve_config_value("$PI_RS_TEST_KEY", Some(&env)).as_deref(),
			Some("secret-value")
		);
		assert_eq!(
			resolve_config_value("${PI_RS_TEST_KEY}", Some(&env)).as_deref(),
			Some("secret-value")
		);
		assert_eq!(
			resolve_config_value("Bearer $PI_RS_TEST_KEY!", Some(&env)).as_deref(),
			Some("Bearer secret-value!")
		);
		// Escapes.
		assert_eq!(
			resolve_config_value("$$not-a-var", Some(&env)).as_deref(),
			Some("$not-a-var")
		);
		assert_eq!(
			resolve_config_value("$!bang", Some(&env)).as_deref(),
			Some("!bang")
		);
		assert_eq!(
			resolve_config_value("cost: $$5", Some(&env)).as_deref(),
			Some("cost: $5")
		);
		// A missing variable leaves the whole value unresolved.
		assert_eq!(resolve_config_value("$PI_RS_MISSING_VAR", Some(&env)), None);
		assert_eq!(
			resolve_config_value("pre-$PI_RS_MISSING_VAR", Some(&env)),
			None
		);
		// An empty value counts as missing (upstream `||` semantics).
		env.insert("PI_RS_EMPTY_VAR".to_string(), String::new());
		assert_eq!(resolve_config_value("$PI_RS_EMPTY_VAR", Some(&env)), None);
		// Invalid ${...} names stay literal.
		assert_eq!(
			resolve_config_value("${not a name}", Some(&env)).as_deref(),
			Some("${not a name}")
		);
		assert_eq!(
			resolve_config_value("${unclosed", Some(&env)).as_deref(),
			Some("${unclosed")
		);
		// Process env path (no injected map): this test's own process env is
		// consulted — nothing under PI_RS_TEST_KEY is set there.
		assert_eq!(resolve_config_value("$PI_RS_TEST_KEY", None), None);
	}

	#[test]
	fn command_expansion_runs_and_caches() {
		let first = resolve_config_value("!printf pi-rs-cmd-key", None);
		assert_eq!(first.as_deref(), Some("pi-rs-cmd-key"));
		// Cached: a second resolution with the same command string is served
		// from the process-lifetime cache.
		let second = resolve_config_value("!printf pi-rs-cmd-key", None);
		assert_eq!(first, second);
		// Non-zero exit resolves to None.
		assert_eq!(resolve_config_value("!false", None), None);
		assert_eq!(resolve_config_value("!exit 3", None), None);
	}

	#[test]
	fn resolve_headers_drops_unresolved_entries() {
		let mut headers = BTreeMap::new();
		headers.insert("X-Good".to_string(), "static".to_string());
		headers.insert("X-Env".to_string(), "$PI_RS_TEST_HDR".to_string());
		headers.insert("X-Bad".to_string(), "$PI_RS_MISSING_HDR".to_string());
		let mut env = BTreeMap::new();
		env.insert("PI_RS_TEST_HDR".to_string(), "injected".to_string());
		let resolved = resolve_headers(&headers, Some(&env));
		assert_eq!(resolved.get("X-Good").map(String::as_str), Some("static"));
		assert_eq!(resolved.get("X-Env").map(String::as_str), Some("injected"));
		assert!(!resolved.contains_key("X-Bad"));
	}

	#[test]
	fn registry_find_and_provider_auth_round_trip() {
		let mut registry = ModelRegistry::load(None);
		let before = registry.models().len();
		registry = ModelRegistry::load(None);
		assert_eq!(registry.models().len(), before);
		assert!(registry.provider_auth("anthropic").is_none());
		let gpt = registry.find("gpt-5.2").unwrap();
		assert_eq!(gpt.provider, "openai");
		registry.set_default(&gpt);
		assert!(registry.default_model().name.contains("(default)"));
	}
}
