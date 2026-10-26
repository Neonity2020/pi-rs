//! Model registry — port of
//! `pi/packages/coding-agent/src/core/model-registry.ts` + `provider-composer.ts`.
//!
//! Framework stage: a small built-in catalog plus models.json custom entries
//! (schema-compatible with pi's `models.json` `providers` section). Virtual
//! models, OAuth providers, and the auth-status composer land with the
//! provider phase.

use serde::Deserialize;

use pi_ai::{Api, Model};

use crate::config;

pub struct ModelRegistry {
	models: Vec<Model>,
}

#[derive(Debug, Deserialize)]
struct ModelsFile {
	#[serde(default)]
	providers: Vec<CustomProvider>,
}

#[derive(Debug, Deserialize)]
struct CustomProvider {
	name: String,
	#[serde(default)]
	api: Option<String>,
	#[serde(default)]
	base_url: Option<String>,
	#[serde(default)]
	models: Vec<CustomModel>,
}

#[derive(Debug, Deserialize)]
struct CustomModel {
	id: String,
	#[serde(default)]
	name: Option<String>,
	#[serde(default)]
	context_window: Option<u64>,
	#[serde(default)]
	max_tokens: Option<u64>,
}

impl ModelRegistry {
	/// Load the catalog: built-ins plus `~/.pi-rs/agent/models.json`.
	pub fn load(settings_default_model: Option<&str>) -> Self {
		let mut models = builtin_models();
		if let Ok(text) = std::fs::read_to_string(config::models_json_path()) {
			if let Ok(file) = serde_json::from_str::<ModelsFile>(&text) {
				for provider in file.providers {
					let api = Api::new(provider.api.unwrap_or_else(|| Api::OPENAI_COMPLETIONS.to_string()));
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
							context_window: model.context_window.unwrap_or(128_000),
							max_tokens: model.max_tokens.unwrap_or(8_192),
						});
					}
				}
			}
		}

		// Default selection: settings default > first env-keyed provider > mock.
		let default_model = settings_default_model
			.map(|s| s.to_string())
			.or_else(|| first_env_model(&models));
		if let Some(default_key) = default_model {
			if let Some(model) = models.iter_mut().find(|m| matches_key(m, &default_key)) {
				model.name = format!("{} (default)", model.name);
			}
		}

		Self { models }
	}

	pub fn models(&self) -> &[Model] {
		&self.models
	}

	pub fn default_model(&self) -> Model {
		if let Some(model) = self.models.iter().find(|m| m.name.ends_with("(default)")) {
			return model.clone();
		}
		if let Some(key) = first_env_model(&self.models) {
			if let Some(model) = self.find(&key) {
				return model;
			}
		}
		self.models.first().expect("registry always contains the mock model").clone()
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
		if let Some(m) = self.models.iter_mut().find(|m| m.id == model.id && m.provider == model.provider) {
			m.name = format!("{} (default)", m.name);
		}
	}
}

fn matches_key(model: &Model, key: &str) -> bool {
	let key = key.replace(" (default)", "");
	model.key() == key || (model.id == key) || format!("{}/{}", model.provider, model.id) == key
}

fn first_env_model(models: &[Model]) -> Option<String> {
	if std::env::var_os("ANTHROPIC_API_KEY").is_some() {
		return models.iter().find(|m| m.provider == "anthropic").map(|m| m.key());
	}
	if std::env::var_os("OPENAI_API_KEY").is_some() {
		return models.iter().find(|m| m.provider == "openai").map(|m| m.key());
	}
	None
}

fn builtin_models() -> Vec<Model> {
	let mut mock = Model::new(
		"mock-echo",
		"Mock (built-in)",
		Api::new("mock"),
		"mock",
		"",
	);
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
	sonnet.cost = pi_ai::ModelCost { input: 3.0, output: 15.0, ..Default::default() };

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

	vec![mock, sonnet, gpt]
}
