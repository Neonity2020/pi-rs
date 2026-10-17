use serde::{Deserialize, Serialize};

/// Token accounting for one request, normalized across providers.
/// `input_tokens` is the total prompt size; `cached_tokens` is the portion
/// served from the provider's prompt cache.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
    pub reasoning_tokens: u64,
}

impl std::ops::AddAssign for Usage {
    fn add_assign(&mut self, rhs: Self) {
        self.input_tokens += rhs.input_tokens;
        self.output_tokens += rhs.output_tokens;
        self.cached_tokens += rhs.cached_tokens;
        self.reasoning_tokens += rhs.reasoning_tokens;
    }
}

/// A price in USD per 1M tokens, with DeepSeek's peak/off-peak split.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Rate {
    pub off_peak: f64,
    pub peak: f64,
}

impl Rate {
    pub fn at(&self, peak: bool) -> f64 {
        if peak {
            self.peak
        } else {
            self.off_peak
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ModelCost {
    pub cache_hit: Rate,
    pub cache_miss: Rate,
    pub output: Rate,
}

/// Model metadata. `cost: None` means "unknown model, no cost estimate".
#[derive(Debug, Clone)]
pub struct Model {
    pub id: String,
    pub provider: String,
    pub context_window: u64,
    pub max_output: u64,
    pub cost: Option<ModelCost>,
}

impl Model {
    pub fn unknown(provider: &str, id: &str) -> Self {
        Self {
            id: id.to_string(),
            provider: provider.to_string(),
            context_window: 0,
            max_output: 0,
            cost: None,
        }
    }

    /// Best-effort cost estimate in USD. `peak` selects DeepSeek peak-hour
    /// pricing; holidays are not accounted for.
    pub fn estimate_cost(&self, usage: &Usage, peak: bool) -> Option<f64> {
        let cost = self.cost.as_ref()?;
        let miss = usage.input_tokens.saturating_sub(usage.cached_tokens) as f64;
        let hit = usage.cached_tokens as f64;
        let out = usage.output_tokens as f64;
        Some(
            (miss * cost.cache_miss.at(peak)
                + hit * cost.cache_hit.at(peak)
                + out * cost.output.at(peak))
                / 1_000_000.0,
        )
    }
}
