//! Per-model cost table (M5 §5.4b) - a consumer of the ADR 003 token counts.
//!
//! Prices come from the operator's config (`cost_per_1m_input`,
//! `cost_per_1m_output`, `cost_per_1k_searches` on each model). A model with
//! no configured price costs 0 - budgets then only bite on priced models,
//! which is the operator's deliberate choice.
//!
//! Prompt caching (ADR 003 amendment): the cache-read and cache-creation
//! parts of a call's input tokens have their own optional rates
//! (`cost_per_1m_cached_input`, `cost_per_1m_cache_write`). Each defaults to
//! the input rate, so a config that sets neither prices exactly as before.

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
    /// Input tokens read from the provider's prompt cache.
    cached_input_token: f64,
    /// Input tokens written to the provider's prompt cache.
    cache_write_token: f64,
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
                    && model.cost_per_1m_cached_input.is_none()
                    && model.cost_per_1m_cache_write.is_none()
                    && model.cost_per_1m_output.is_none()
                    && model.cost_per_1k_searches.is_none()
                {
                    continue;
                }
                // An unset cache rate is the input rate: no discount and no
                // surcharge the operator did not ask for.
                let input = model.cost_per_1m_input.unwrap_or(0.0);
                prices.insert(
                    model.id.clone(),
                    ModelPrice {
                        input_token: input / 1_000_000.0,
                        cached_input_token: model.cost_per_1m_cached_input.unwrap_or(input)
                            / 1_000_000.0,
                        cache_write_token: model.cost_per_1m_cache_write.unwrap_or(input)
                            / 1_000_000.0,
                        output_token: model.cost_per_1m_output.unwrap_or(0.0) / 1_000_000.0,
                        search_unit: model.cost_per_1k_searches.unwrap_or(0.0) / 1_000.0,
                    },
                );
            }
        }
        Self { prices }
    }

    /// Cost of a chat call (also correct for embeddings with
    /// `tokens_out = 0`), every input token at the input rate. Use
    /// [`token_cost_with_cache`](Self::token_cost_with_cache) when the
    /// upstream reported a cache breakdown.
    #[must_use]
    pub fn token_cost(&self, model: &str, tokens_in: u64, tokens_out: u64) -> f64 {
        self.token_cost_with_cache(model, tokens_in, tokens_out, None, None)
    }

    /// Cost of a chat call whose upstream reported how many of its input
    /// tokens were read from (`cached`) or written to (`cache_write`) the
    /// prompt cache.
    ///
    /// Both counts are subsets of `tokens_in` (OpenAI's
    /// `prompt_tokens_details.cached_tokens`; Anthropic's cache read and
    /// cache creation, which the provider layer already folds into the
    /// prompt count). They are charged at the model's cached-input and
    /// cache-write rates and the rest of `tokens_in` at the input rate. A
    /// breakdown larger than `tokens_in` is clamped, so the three parts
    /// always sum to `tokens_in` and a malformed upstream count can never
    /// produce a negative or inflated input.
    #[must_use]
    pub fn token_cost_with_cache(
        &self,
        model: &str,
        tokens_in: u64,
        tokens_out: u64,
        cached: Option<u64>,
        cache_write: Option<u64>,
    ) -> f64 {
        self.prices.get(model).map_or(0.0, |p| {
            let cached = cached.unwrap_or(0).min(tokens_in);
            let cache_write = cache_write.unwrap_or(0).min(tokens_in - cached);
            let plain = tokens_in - cached - cache_write;
            to_f64(plain) * p.input_token
                + to_f64(cached) * p.cached_input_token
                + to_f64(cache_write) * p.cache_write_token
                + to_f64(tokens_out) * p.output_token
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
        id = "openai/gpt"
        capabilities = ["chat"]
        cost_per_1m_input = 2.0
        cost_per_1m_output = 10.0
        [[providers.models]]
        id = "openai/rr"
        capabilities = ["rerank"]
        cost_per_1k_searches = 2.0
    "#;

    #[test]
    fn chat_cost_combines_both_directions() {
        let t = table(PRICED);
        // 1M in at $2 + 100k out at $10 = 2 + 1 = 3.
        let cost = t.token_cost("openai/gpt", 1_000_000, 100_000);
        assert!((cost - 3.0).abs() < 1e-9, "{cost}");
    }

    const CACHE_PRICED: &str = r#"
        [[providers]]
        name = "anthropic"
        kind = "anthropic"
        [[providers.models]]
        id = "anthropic/claude"
        capabilities = ["chat"]
        cost_per_1m_input = 3.0
        cost_per_1m_output = 15.0
        cost_per_1m_cached_input = 0.3
        cost_per_1m_cache_write = 3.75
        [[providers.models]]
        id = "anthropic/read-only"
        capabilities = ["chat"]
        cost_per_1m_input = 3.0
        cost_per_1m_cached_input = 0.3
        [[providers.models]]
        id = "anthropic/cache-only"
        capabilities = ["chat"]
        cost_per_1m_cached_input = 0.3
    "#;

    #[test]
    fn cached_and_cache_write_tokens_have_their_own_rates() {
        let t = table(CACHE_PRICED);
        // 1M input: 200k plain at $3, 700k read at $0.30, 100k written at
        // $3.75, plus 100k out at $15.
        let cost = t.token_cost_with_cache(
            "anthropic/claude",
            1_000_000,
            100_000,
            Some(700_000),
            Some(100_000),
        );
        let expected = 0.6 + 0.21 + 0.375 + 1.5;
        assert!((cost - expected).abs() < 1e-9, "{cost}");
    }

    #[test]
    fn unset_cache_rates_default_to_the_input_rate() {
        // No cache rate at all: the breakdown changes nothing.
        let t = table(PRICED);
        let flat = t.token_cost("openai/gpt", 1_000_000, 100_000);
        let with_breakdown = t.token_cost_with_cache(
            "openai/gpt",
            1_000_000,
            100_000,
            Some(600_000),
            Some(200_000),
        );
        assert!(
            (flat - with_breakdown).abs() < 1e-12,
            "{flat} {with_breakdown}"
        );

        // Only the read rate set: written tokens stay at the input rate.
        let t = table(CACHE_PRICED);
        let cost = t.token_cost_with_cache(
            "anthropic/read-only",
            1_000_000,
            0,
            Some(500_000),
            Some(500_000),
        );
        assert!((cost - (0.15 + 1.5)).abs() < 1e-9, "{cost}");

        // A cache rate with no input rate is still a priced model.
        let cost =
            t.token_cost_with_cache("anthropic/cache-only", 1_000_000, 0, Some(1_000_000), None);
        assert!((cost - 0.3).abs() < 1e-9, "{cost}");
    }

    #[test]
    fn no_breakdown_is_the_plain_token_cost() {
        let t = table(CACHE_PRICED);
        let plain = t.token_cost("anthropic/claude", 1_000_000, 100_000);
        assert!((plain - 4.5).abs() < 1e-9, "{plain}");
        let none = t.token_cost_with_cache("anthropic/claude", 1_000_000, 100_000, None, None);
        assert!((plain - none).abs() < 1e-12);
    }

    #[test]
    fn a_breakdown_larger_than_the_input_is_clamped() {
        let t = table(CACHE_PRICED);
        // 100 input tokens, but the upstream claims 80 read and 50 written:
        // 80 read, 20 written, nothing left at the plain rate.
        let cost = t.token_cost_with_cache("anthropic/claude", 100, 0, Some(80), Some(50));
        let expected = (80.0 * 0.3 + 20.0 * 3.75) / 1_000_000.0;
        assert!((cost - expected).abs() < 1e-12, "{cost}");
        // More cached than input: everything at the cached rate, never more.
        let cost = t.token_cost_with_cache("anthropic/claude", 100, 0, Some(500), Some(500));
        assert!((cost - 100.0 * 0.3 / 1_000_000.0).abs() < 1e-12, "{cost}");
    }

    #[test]
    fn rerank_cost_uses_search_units() {
        let t = table(PRICED);
        let cost = t.search_cost("openai/rr", 500);
        assert!((cost - 1.0).abs() < 1e-9, "{cost}");
    }

    #[test]
    fn unpriced_model_costs_zero() {
        let t = table(PRICED);
        assert!(t.token_cost("unknown", 1_000_000, 1_000_000).abs() < f64::EPSILON);
        assert!(t.search_cost("gpt", 10).abs() < f64::EPSILON);
    }
}
