//! DeepSeek specifics: base URL, model registry with pricing, peak-hour logic.
//!
//! DeepSeek exposes an OpenAI-compatible chat completions API at
//! `https://api.deepseek.com` with two extra knobs: `thinking` and
//! `reasoning_effort`.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::model::{Model, ModelCost, Rate};

pub const BASE_URL: &str = "https://api.deepseek.com";
pub const API_KEY_ENV: &str = "DEEPSEEK_API_KEY";
pub const DEFAULT_MODEL: &str = "deepseek-flash";

fn rate(off_peak: f64, peak: f64) -> Rate {
    Rate { off_peak, peak }
}

/// Known models with current pricing (USD / 1M tokens, off-peak & peak).
pub fn models() -> Vec<Model> {
    vec![
        Model {
            id: "deepseek-flash".into(),
            provider: "deepseek".into(),
            context_window: 1_000_000,
            max_output: 384_000,
            cost: Some(ModelCost {
                cache_hit: rate(0.003, 0.006),
                cache_miss: rate(0.15, 0.3),
                output: rate(0.6, 1.2),
            }),
        },
        Model {
            id: "deepseek-v4-pro".into(),
            provider: "deepseek".into(),
            context_window: 1_000_000,
            max_output: 384_000,
            cost: Some(ModelCost {
                cache_hit: rate(0.022, 0.044),
                cache_miss: rate(0.66, 1.32),
                output: rate(1.98, 3.96),
            }),
        },
    ]
}

pub fn model(id: &str) -> Option<Model> {
    models().into_iter().find(|m| m.id == id)
}

pub fn model_or_unknown(id: &str) -> Model {
    model(id).unwrap_or_else(|| Model::unknown("deepseek", id))
}

/// DeepSeek peak hours: 01:00–04:00 and 06:00–10:00 UTC, Monday–Friday.
/// Chinese public holidays are not accounted for; this is a cost *estimate*.
pub fn is_peak_now() -> bool {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    is_peak_at(secs)
}

fn is_peak_at(unix_secs: u64) -> bool {
    let days = unix_secs / 86_400;
    let hour = (unix_secs % 86_400) / 3_600;
    // 1970-01-01 was a Thursday; shift so 0 = Sunday.
    let weekday = (days + 4) % 7;
    let is_weekday = (1..=5).contains(&weekday);
    let in_peak_window = (1..4).contains(&hour) || (6..10).contains(&hour);
    is_weekday && in_peak_window
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peak_windows() {
        // 1970-01-01 (Thursday) 00:00 UTC — not peak.
        assert!(!is_peak_at(0));
        // 02:00 UTC Thursday — peak.
        assert!(is_peak_at(2 * 3600));
        // 05:00 UTC Thursday — between windows, not peak.
        assert!(!is_peak_at(5 * 3600));
        // 07:30 UTC Thursday — peak.
        assert!(is_peak_at(7 * 3600 + 1800));
        // 10:00 UTC Thursday — window ends, not peak.
        assert!(!is_peak_at(10 * 3600));
        // 02:00 UTC Saturday (1970-01-03) — weekend, not peak.
        assert!(!is_peak_at(2 * 86_400 + 2 * 3600));
    }

    #[test]
    fn known_model_costs() {
        let m = model("deepseek-flash").unwrap();
        let usage = crate::Usage {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            cached_tokens: 500_000,
            reasoning_tokens: 0,
        };
        let cost = m.estimate_cost(&usage, false).unwrap();
        // 0.5M cache miss * $0.15 + 0.5M cache hit * $0.003 + 1M output * $0.6
        let expected = 0.5 * 0.15 + 0.5 * 0.003 + 0.6;
        assert!((cost - expected).abs() < 1e-9, "{cost} != {expected}");
    }
}
