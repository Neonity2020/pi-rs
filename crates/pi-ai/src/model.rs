//! Model catalog types — port of `pi/packages/ai/src/types.ts` (model section)
//! and `providers/model-schema.ts`.

use serde::{Deserialize, Serialize};

/// API identifier matching the TS pi's `KnownApi` string union
/// (`"anthropic-messages"`, `"openai-completions"`, ...). Unknown custom
/// adapters are arbitrary strings, mirroring `Api = KnownApi | string`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Api(pub String);

impl Api {
	pub const ANTHROPIC_MESSAGES: &'static str = "anthropic-messages";
	pub const OPENAI_COMPLETIONS: &'static str = "openai-completions";
	pub const OPENAI_RESPONSES: &'static str = "openai-responses";
	pub const MISTRAL_CONVERSATIONS: &'static str = "mistral-conversations";
	pub const BEDROCK_CONVERSE_STREAM: &'static str = "bedrock-converse-stream";
	pub const GOOGLE_GENERATIVE_AI: &'static str = "google-generative-ai";
	pub const GOOGLE_VERTEX: &'static str = "google-vertex";
	pub const PI_MESSAGES: &'static str = "pi-messages";

	pub fn new(name: impl Into<String>) -> Self {
		Api(name.into())
	}

	pub fn as_str(&self) -> &str {
		&self.0
	}
}

impl std::fmt::Display for Api {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str(&self.0)
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum InputModality {
	Text,
	Image,
	Audio,
	Video,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ModelCost {
	/// USD per 1M input tokens.
	#[serde(default)]
	pub input: f64,
	/// USD per 1M output tokens.
	#[serde(default)]
	pub output: f64,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub cache_read: Option<f64>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub cache_write: Option<f64>,
}

/// Chat model usable with `stream()` and friends.
///
/// Mirrors `Model<Api>` from the TS pi; provider-specific `compat` overrides
/// are ported together with the concrete adapters.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Model {
	pub id: String,
	pub name: String,
	pub api: Api,
	pub provider: String,
	pub base_url: String,
	#[serde(default)]
	pub reasoning: bool,
	#[serde(default)]
	pub input: Vec<InputModality>,
	#[serde(default)]
	pub cost: ModelCost,
	/// Total context window in tokens.
	#[serde(default)]
	pub context_window: u64,
	/// Max output tokens per response.
	#[serde(default)]
	pub max_tokens: u64,
}

impl Model {
	pub fn new(
		id: impl Into<String>,
		name: impl Into<String>,
		api: impl Into<Api>,
		provider: impl Into<String>,
		base_url: impl Into<String>,
	) -> Self {
		Self {
			id: id.into(),
			name: name.into(),
			api: api.into(),
			provider: provider.into(),
			base_url: base_url.into(),
			reasoning: false,
			input: vec![InputModality::Text],
			cost: ModelCost::default(),
			context_window: 0,
			max_tokens: 0,
		}
	}

	/// Catalog key used in UIs and settings: `provider/model-id`.
	pub fn key(&self) -> String {
		format!("{}/{}", self.provider, self.id)
	}

	pub fn supports_images(&self) -> bool {
		self.input.contains(&InputModality::Image)
	}
}

impl std::fmt::Display for Model {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str(&self.key())
	}
}

impl From<&'static str> for Api {
	fn from(s: &'static str) -> Self {
		Api(s.to_string())
	}
}
