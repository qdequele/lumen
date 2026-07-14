//! Per-model cost table (M5 §5.4b) - a consumer of the ADR 003 token counts.
//!
//! Prices come from the operator's config (`cost_per_1m_input`,
//! `cost_per_1m_output`, `cost_per_1k_searches` on each model). A model with
//! no configured price costs 0 - budgets then only bite on priced models,
//! which is the operator's deliberate choice.

use crate::config::Config;
use std::collections::HashMap;

/// Reserved output tokens for the pre-call budget estimate when the client
/// did not send `max_tokens` (the reservation is corrected to the real usage
/// after the call).
pub const DEFAULT_RESERVED_OUTPUT_TOKENS: u64 = 2_048;

/// Unit prices in USD (per token / per search unit).
#[derive(Debug, Clone, Copy, Default)]
struct ModelPrice {
    input_token: f64,
    output_token: f64,
    search_unit: f64,
}

/// Price lookup by client-facing model id.
#[derive(Debug, Default)]
pub struct CostTable {
    prices: HashMap<String, ModelPrice>,
}

impl CostTable {
    /// Build the table from every priced model in the config.
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        let mut prices = HashMap::new();
        for provider in &config.providers {
            for model in &provider.models {
                if model.cost_per_1m_input.is_none()
                    && model.cost_per_1m_output.is_none()
                    && model.cost_per_1k_searches.is_none()
                {
                    continue;
                }
                prices.insert(
                    model.id.clone(),
                    ModelPrice {
                        input_token: model.cost_per_1m_input.unwrap_or(0.0) / 1_000_000.0,
                        output_token: model.cost_per_1m_output.unwrap_or(0.0) / 1_000_000.0,
                        search_unit: model.cost_per_1k_searches.unwrap_or(0.0) / 1_000.0,
                    },
                );
            }
        }
        Self { prices }
    }

    /// Cost of a chat call (also correct for embeddings with
    /// `tokens_out = 0`).
    #[must_use]
    pub fn token_cost(&self, model: &str, tokens_in: u64, tokens_out: u64) -> f64 {
        self.prices.get(model).map_or(0.0, |p| {
            to_f64(tokens_in) * p.input_token + to_f64(tokens_out) * p.output_token
        })
    }

    /// Cost of a rerank call, billed in search units.
    #[must_use]
    pub fn search_cost(&self, model: &str, search_units: u64) -> f64 {
        self.prices
            .get(model)
            .map_or(0.0, |p| to_f64(search_units) * p.search_unit)
    }
}

/// Token counts are far below 2^53; the conversion is exact in practice.
#[allow(clippy::cast_precision_loss)]
fn to_f64(count: u64) -> f64 {
    count as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use figment::providers::{Format, Toml};
    use figment::Figment;

    fn table(toml: &str) -> CostTable {
        let config: Config = Figment::new()
            .merge(Toml::string(toml))
            .extract()
            .expect("valid config");
        CostTable::from_config(&config)
    }

    const PRICED: &str = r#"
        [[providers]]
        name = "openai"
        kind = "openai"
        [[providers.models]]
        id = "gpt"
        capabilities = ["chat"]
        cost_per_1m_input = 2.0
        cost_per_1m_output = 10.0
        [[providers.models]]
        id = "rr"
        capabilities = ["rerank"]
        cost_per_1k_searches = 2.0
    "#;

    #[test]
    fn chat_cost_combines_both_directions() {
        let t = table(PRICED);
        // 1M in at $2 + 100k out at $10 = 2 + 1 = 3.
        let cost = t.token_cost("gpt", 1_000_000, 100_000);
        assert!((cost - 3.0).abs() < 1e-9, "{cost}");
    }

    #[test]
    fn rerank_cost_uses_search_units() {
        let t = table(PRICED);
        let cost = t.search_cost("rr", 500);
        assert!((cost - 1.0).abs() < 1e-9, "{cost}");
    }

    #[test]
    fn unpriced_model_costs_zero() {
        let t = table(PRICED);
        assert!(t.token_cost("unknown", 1_000_000, 1_000_000).abs() < f64::EPSILON);
        assert!(t.search_cost("gpt", 10).abs() < f64::EPSILON);
    }
}
