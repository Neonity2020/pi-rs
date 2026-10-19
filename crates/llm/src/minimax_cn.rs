//! MiniMax (China) specifics: M Plan subscription endpoint and model registry.
//!
//! The subscription key (`sk-cp-…`) works against MiniMax's OpenAI-compatible
//! endpoint at `https://api.minimax.cn/v1`. M Plan is billed per subscription,
//! not per token, so the registry carries context limits but no prices and
//! stats show the cost as `-`.

use crate::model::Model;

pub const BASE_URL: &str = "https://api.minimax.cn/v1";
pub const API_KEY_ENV: &str = "MINIMAX_CN_API_KEY";
pub const DEFAULT_MODEL: &str = "MiniMax-M3.1-Flash";

/// Names MiniMax uses in its docs that the API does not accept. The M3.1
/// generation is served as `MiniMax-M3.1-Flash`; sending `MiniMax-M3.1`
/// verbatim fails with `unknown model`.
const ALIASES: &[(&str, &str)] = &[("MiniMax-M3.1", "MiniMax-M3.1-Flash")];

/// Resolve a user-supplied model name to the ID the API expects.
pub fn canonical_id(id: &str) -> &str {
    ALIASES
        .iter()
        .find(|(alias, _)| *alias == id)
        .map(|(_, real)| *real)
        .unwrap_or(id)
}

fn model_entry(id: &str, context_window: u64, max_output: u64) -> Model {
    Model {
        id: id.into(),
        provider: "minimax-cn".into(),
        context_window,
        max_output,
        cost: None,
    }
}

/// Currently documented text models. Older M2.x IDs still work as unknown
/// models; they simply do not drive automatic compaction.
pub fn models() -> Vec<Model> {
    vec![
        model_entry("MiniMax-M3.1-Flash", 1_000_000, 524_288),
        model_entry("MiniMax-M3", 1_000_000, 524_288),
    ]
}

pub fn model(id: &str) -> Option<Model> {
    models().into_iter().find(|m| m.id == canonical_id(id))
}

pub fn model_or_unknown(id: &str) -> Model {
    model(id).unwrap_or_else(|| Model::unknown("minimax-cn", id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscription_model_is_known() {
        let model = model_or_unknown(DEFAULT_MODEL);
        assert_eq!(model.provider, "minimax-cn");
        assert_eq!(model.context_window, 1_000_000);
        assert_eq!(model.max_output, 524_288);
        assert!(model.cost.is_none(), "M Plan is subscription-billed");
    }

    #[test]
    fn unknown_models_still_resolve() {
        let model = model_or_unknown("MiniMax-M2.7");
        assert_eq!(model.provider, "minimax-cn");
        assert_eq!(model.context_window, 0);
    }

    #[test]
    fn the_m3_1_alias_resolves_to_the_flash_model() {
        assert_eq!(canonical_id("MiniMax-M3.1"), "MiniMax-M3.1-Flash");
        let model = model_or_unknown("MiniMax-M3.1");
        assert_eq!(model.id, "MiniMax-M3.1-Flash");
        assert_eq!(model.context_window, 1_000_000);
    }

    #[test]
    fn the_wire_id_is_the_one_the_api_accepts() {
        // `MiniMax-M3.1` is rejected by the API with `unknown model`, so the
        // registry must hand out the real ID.
        assert_eq!(model_or_unknown("MiniMax-M3.1").id, DEFAULT_MODEL);
        assert_eq!(canonical_id("MiniMax-M3"), "MiniMax-M3");
        assert_eq!(canonical_id("MiniMax-M2.7"), "MiniMax-M2.7");
    }
}
