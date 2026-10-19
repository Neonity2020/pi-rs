//! pi-llm — the provider layer of pi-rs.
//!
//! One job: turn a `ChatRequest` into a stream of `StreamEvent`s.
//! It knows nothing about tools, sessions, prompts, or terminals.
//!
//! Layers: `coding -> agent -> llm`.

pub mod cancel;
pub mod deepseek;
pub mod error;
pub mod message;
pub mod minimax_cn;
pub mod mock;
pub mod model;
pub mod openai;
pub mod provider;

pub use cancel::Cancel;
pub use error::Error;
pub use message::{Image, Message, Role, ToolCall};
pub use model::{Model, ModelCost, Rate, Usage};
pub use openai::OpenAiCompat;
pub use provider::{
    BoxFuture, ChatRequest, Effort, Provider, StopReason, StreamEvent, Thinking, ToolSpec,
};

/// Every model the given provider can serve, in registry order.
///
/// `openai` has no registry: any OpenAI-compatible endpoint may name its own
/// models, so the list is empty and unknown ids still work.
pub fn provider_models(provider: &str) -> Vec<Model> {
    match provider {
        "deepseek" => deepseek::models(),
        "minimax-cn" => minimax_cn::models(),
        _ => Vec::new(),
    }
}

/// Resolve a model id for a provider. Known ids get their registry entry
/// (context window, output limit, price); unknown ones still work, they just
/// carry no metadata.
pub fn provider_model(provider: &str, id: &str) -> Model {
    match provider {
        "deepseek" => deepseek::model_or_unknown(id),
        "minimax-cn" => minimax_cn::model_or_unknown(id),
        other => Model::unknown(other, id),
    }
}

/// Providers that keep a model registry, in listing order. `openai` is not
/// one: it names whatever models the endpoint serves.
pub const REGISTRY_PROVIDERS: &[&str] = &["deepseek", "minimax-cn"];

/// Every known model, with its provider.
pub fn all_models() -> Vec<Model> {
    REGISTRY_PROVIDERS
        .iter()
        .flat_map(|provider| provider_models(provider))
        .collect()
}

/// The provider that serves `id`, if a registry knows it. Model ids are
/// unique across registries, so this is the routing table for `/model`.
/// Aliases count: `MiniMax-M3.1` resolves to the MiniMax CN registry.
pub fn provider_of_model(id: &str) -> Option<&'static str> {
    REGISTRY_PROVIDERS.iter().copied().find(|provider| {
        let canonical = canonical_id(provider, id);
        provider_models(provider)
            .iter()
            .any(|model| model.id == canonical)
    })
}

/// The id the API accepts for a provider's model name.
fn canonical_id<'a>(provider: &str, id: &'a str) -> &'a str {
    match provider {
        "minimax-cn" => minimax_cn::canonical_id(id),
        _ => id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_registry_model_is_listed() {
        let all = all_models();
        assert!(all.iter().any(|m| m.id == "deepseek-flash"));
        assert!(all.iter().any(|m| m.id == "MiniMax-M3.1-Flash"));
        // OpenAI-compatible endpoints name their own models; no registry.
        assert!(provider_models("openai").is_empty());
    }

    #[test]
    fn a_model_id_routes_to_its_provider() {
        assert_eq!(provider_of_model("deepseek-flash"), Some("deepseek"));
        assert_eq!(provider_of_model("deepseek-v4-pro"), Some("deepseek"));
        assert_eq!(provider_of_model("MiniMax-M3"), Some("minimax-cn"));
        // The family name resolves to the same registry.
        assert_eq!(provider_of_model("MiniMax-M3.1"), Some("minimax-cn"));
        assert_eq!(provider_of_model("nope"), None);
    }

    #[test]
    fn resolving_a_model_yields_the_wire_id() {
        let model = provider_model("minimax-cn", "MiniMax-M3.1");
        assert_eq!(model.id, "MiniMax-M3.1-Flash");
        assert_eq!(model.provider, "minimax-cn");
        // Unknown ids still work, they just carry no metadata.
        let unknown = provider_model("openai", "gpt-9");
        assert_eq!(unknown.id, "gpt-9");
        assert_eq!(unknown.context_window, 0);
    }
}
