use llm::{deepseek, Model, Usage};

/// Token and cost totals for the process, updated from usage events.
#[derive(Debug, Default, Clone)]
pub struct Stats {
    pub usage: Usage,
    pub cost_usd: f64,
}

impl Stats {
    pub fn add(&mut self, usage: Usage, model: &Model) {
        self.usage += usage;
        if let Some(cost) = model.estimate_cost(&usage, deepseek::is_peak_now()) {
            self.cost_usd += cost;
        }
    }

    pub fn cost_display(&self, model: &Model) -> String {
        // A model without a price (a subscription plan) cannot be estimated,
        // but usage on a priced model still cost money — so only show `-`
        // when there is nothing to show.
        if model.cost.is_none() && self.cost_usd <= 0.0 {
            "-".to_string()
        } else {
            format!("${:.4}", self.cost_usd)
        }
    }
}

/// 12345 → "12.3k"
pub fn format_tokens(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 1_000_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    }
}
