//! Server-side resilience runtime (M6): the process-wide circuit breakers plus
//! the resolved retry policy, timeouts and fallback chains derived from config.
//!
//! This is the glue between [`Config`](crate::config::Config) and the router's
//! [`executor`](ferrogate_router::executor): the handlers ask it for a model's
//! fallback chain ([`chain_ids`](ResilienceRuntime::chain_ids)) and the
//! per-model execution knobs ([`exec_config`](ResilienceRuntime::exec_config)).
//! All state is in-memory; nothing here touches a database.

use std::collections::HashMap;
use std::time::Duration;

use axum::http::{HeaderMap, HeaderName, HeaderValue};
use ferrogate_router::circuit::{BreakerConfig, CircuitBreakers};
use ferrogate_router::executor::ExecConfig;
use ferrogate_router::retry::RetryPolicy;
use ferrogate_telemetry::ResilienceMetrics;

use crate::config::Config;

/// The `x-ferrogate-model-used` response header name (M6 §6.2).
const MODEL_USED_HEADER: &str = "x-ferrogate-model-used";

/// A one-header [`HeaderMap`] advertising the model that actually served the
/// request. Skips the header rather than failing if the id is not a valid
/// header value (model ids are operator-defined and normally are).
#[must_use]
pub fn model_used_headers(model_used: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if let Ok(value) = HeaderValue::from_str(model_used) {
        headers.insert(HeaderName::from_static(MODEL_USED_HEADER), value);
    }
    headers
}

/// The two request-scoped timeouts the executor enforces (connect is a
/// client-wide setting, applied when the HTTP client is built).
#[derive(Debug, Clone, Copy)]
pub struct Timeouts {
    /// Time to the upstream's first sign of life (FG-3011).
    pub first_token: Duration,
    /// Overall cap on the whole call, all retries + fallbacks (FG-3013).
    pub total: Duration,
}

/// Process-wide resilience state and resolved policy.
#[derive(Debug)]
pub struct ResilienceRuntime {
    /// Per-(provider, model) circuit breakers.
    pub breakers: CircuitBreakers,
    retry: RetryPolicy,
    default_timeouts: Timeouts,
    /// Per-model timeout overrides (inherited from the owning provider).
    model_timeouts: HashMap<String, Timeouts>,
    /// Per-model ordered fallback chains (excludes the primary).
    fallbacks: HashMap<String, Vec<String>>,
}

impl ResilienceRuntime {
    /// Build from config, wiring circuit-state transitions to `metrics` when
    /// provided.
    #[must_use]
    pub fn from_config(config: &Config, metrics: Option<ResilienceMetrics>) -> Self {
        let r = &config.resilience;
        let retry = RetryPolicy {
            max_attempts: r.retry_max_attempts,
            base: Duration::from_millis(r.retry_base_ms),
            max: Duration::from_millis(r.retry_max_ms),
        };
        let breaker_config = BreakerConfig {
            failure_threshold: r.circuit_failure_threshold,
            cooldown: Duration::from_millis(r.circuit_cooldown_ms),
        };
        let default_timeouts = Timeouts {
            first_token: Duration::from_millis(config.server.first_token_timeout_ms),
            total: Duration::from_millis(r.total_timeout_ms),
        };
        let model_timeouts = config
            .model_timeout_overrides()
            .into_iter()
            .map(|(model, (first_token, total))| {
                (
                    model,
                    Timeouts {
                        first_token: first_token
                            .map_or(default_timeouts.first_token, Duration::from_millis),
                        total: total.map_or(default_timeouts.total, Duration::from_millis),
                    },
                )
            })
            .collect();
        Self {
            breakers: CircuitBreakers::new(breaker_config, metrics),
            retry,
            default_timeouts,
            model_timeouts,
            fallbacks: config.fallback_map(),
        }
    }

    /// A runtime with library defaults, no fallbacks and no gauge — used by
    /// tests and as the open-gateway baseline.
    #[must_use]
    pub fn defaults() -> Self {
        Self {
            breakers: CircuitBreakers::new(BreakerConfig::default(), None),
            retry: RetryPolicy::default(),
            default_timeouts: Timeouts {
                first_token: Duration::from_secs(30),
                total: Duration::from_secs(600),
            },
            model_timeouts: HashMap::new(),
            fallbacks: HashMap::new(),
        }
    }

    /// Override the default first-token timeout (builder style). Used by tests
    /// and by callers that drive the executor without a full config.
    #[must_use]
    pub fn with_first_token(mut self, first_token: Duration) -> Self {
        self.default_timeouts.first_token = first_token;
        self
    }

    /// Override the retry policy (builder style) — e.g. tests that assert on a
    /// single attempt.
    #[must_use]
    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// The ordered chain of client-facing model ids to try for `model`: the
    /// model itself first, then its configured fallbacks.
    #[must_use]
    pub fn chain_ids(&self, model: &str) -> Vec<String> {
        let mut ids = Vec::with_capacity(1 + self.fallbacks.get(model).map_or(0, Vec::len));
        ids.push(model.to_owned());
        if let Some(fallbacks) = self.fallbacks.get(model) {
            ids.extend(fallbacks.iter().cloned());
        }
        ids
    }

    /// The execution knobs (retry + timeouts) for `model`, applying the
    /// per-model timeout override when present.
    #[must_use]
    pub fn exec_config(&self, model: &str) -> ExecConfig {
        let t = self
            .model_timeouts
            .get(model)
            .copied()
            .unwrap_or(self.default_timeouts);
        ExecConfig {
            retry: self.retry,
            first_token: t.first_token,
            total: t.total,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use figment::{
        providers::{Format, Toml},
        Figment,
    };

    fn load(toml: &str) -> Config {
        let figment = Figment::new().merge(Toml::string(toml));
        figment.extract::<Config>().expect("valid config")
    }

    #[test]
    fn chain_ids_is_primary_then_fallbacks() {
        let cfg = load(
            r#"
            [[providers]]
            name = "openai"
            kind = "openai"
            [[providers.models]]
            id = "gpt"
            capabilities = ["chat"]
            fallbacks = ["claude", "mistral"]
            [[providers]]
            name = "anthropic"
            kind = "anthropic"
            [[providers.models]]
            id = "claude"
            capabilities = ["chat"]
            [[providers]]
            name = "mistral"
            kind = "mistral"
            [[providers.models]]
            id = "mistral"
            capabilities = ["chat"]
        "#,
        );
        let rt = ResilienceRuntime::from_config(&cfg, None);
        assert_eq!(rt.chain_ids("gpt"), vec!["gpt", "claude", "mistral"]);
        // No fallbacks → just the model itself.
        assert_eq!(rt.chain_ids("claude"), vec!["claude"]);
    }

    #[test]
    fn exec_config_applies_per_provider_timeout_override() {
        let cfg = load(
            r#"
            [server]
            first_token_timeout_ms = 30000
            [resilience]
            total_timeout_ms = 600000
            [[providers]]
            name = "slow"
            kind = "openai"
            first_token_timeout_ms = 90000
            [[providers.models]]
            id = "gpt"
            capabilities = ["chat"]
            [[providers]]
            name = "fast"
            kind = "anthropic"
            [[providers.models]]
            id = "claude"
            capabilities = ["chat"]
        "#,
        );
        let rt = ResilienceRuntime::from_config(&cfg, None);
        // Overridden first-token, default total.
        let slow = rt.exec_config("gpt");
        assert_eq!(slow.first_token, Duration::from_secs(90));
        assert_eq!(slow.total, Duration::from_secs(600));
        // No override → global default.
        let fast = rt.exec_config("claude");
        assert_eq!(fast.first_token, Duration::from_secs(30));
    }
}
