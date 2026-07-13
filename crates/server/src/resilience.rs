//! Server-side resilience runtime (M6): the process-wide circuit breakers plus
//! the resolved retry policy, timeouts and fallback chains derived from config.
//!
//! This is the glue between [`Config`](crate::config::Config) and the router's
//! [`executor`](lumen_router::executor): the handlers ask it for a model's
//! fallback chain ([`chain_ids`](ResilienceRuntime::chain_ids)) and the
//! per-model execution knobs ([`exec_config`](ResilienceRuntime::exec_config)).
//! All state is in-memory; nothing here touches a database.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::http::{HeaderMap, HeaderName, HeaderValue};
use lumen_router::circuit::{BreakerConfig, CircuitBreakers};
use lumen_router::executor::ExecConfig;
use lumen_router::retry::RetryPolicy;
use lumen_telemetry::ResilienceMetrics;

use crate::config::Config;

/// The `x-lumen-model-used` response header name (M6 §6.2).
const MODEL_USED_HEADER: &str = "x-lumen-model-used";

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
    /// Time to the upstream's first sign of life (LM-3011).
    pub first_token: Duration,
    /// Overall cap on the whole call, all retries + fallbacks (LM-3013).
    pub total: Duration,
}

/// The hot-swappable part of the resilience config: everything derived from the
/// config file. The circuit breakers are deliberately *not* here — their live
/// state must survive a reload (a reload must not reset an open circuit).
#[derive(Debug, Clone)]
struct ResiliencePolicy {
    retry: RetryPolicy,
    default_timeouts: Timeouts,
    /// Per-model timeout overrides (inherited from the owning provider).
    model_timeouts: HashMap<String, Timeouts>,
    /// Per-model ordered fallback chains (excludes the primary).
    fallbacks: HashMap<String, Vec<String>>,
}

impl ResiliencePolicy {
    fn from_config(config: &Config) -> Self {
        let r = &config.resilience;
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
            retry: RetryPolicy {
                max_attempts: r.retry_max_attempts,
                base: Duration::from_millis(r.retry_base_ms),
                max: Duration::from_millis(r.retry_max_ms),
            },
            default_timeouts,
            model_timeouts,
            fallbacks: config.fallback_map(),
        }
    }

    fn defaults() -> Self {
        Self {
            retry: RetryPolicy::default(),
            default_timeouts: Timeouts {
                first_token: Duration::from_secs(30),
                total: Duration::from_secs(600),
            },
            model_timeouts: HashMap::new(),
            fallbacks: HashMap::new(),
        }
    }
}

/// Process-wide resilience state. The circuit breakers are stable for the life
/// of the process (so their state survives a hot reload); the derived policy is
/// behind an [`ArcSwap`] so a config reload can replace it atomically without
/// touching breaker state (DEBT-1 / M7 §7.3).
#[derive(Debug)]
pub struct ResilienceRuntime {
    /// Per-(provider, model) circuit breakers — never rebuilt on reload.
    pub breakers: CircuitBreakers,
    policy: arc_swap::ArcSwap<ResiliencePolicy>,
}

impl ResilienceRuntime {
    /// Build from config, wiring circuit-state transitions to `metrics` when
    /// provided.
    #[must_use]
    pub fn from_config(config: &Config, metrics: Option<ResilienceMetrics>) -> Self {
        let r = &config.resilience;
        let breaker_config = BreakerConfig {
            failure_threshold: r.circuit_failure_threshold,
            cooldown: Duration::from_millis(r.circuit_cooldown_ms),
        };
        Self {
            breakers: CircuitBreakers::new(breaker_config, metrics),
            policy: arc_swap::ArcSwap::from_pointee(ResiliencePolicy::from_config(config)),
        }
    }

    /// A runtime with library defaults, no fallbacks and no gauge — used by
    /// tests and as the open-gateway baseline.
    #[must_use]
    pub fn defaults() -> Self {
        Self {
            breakers: CircuitBreakers::new(BreakerConfig::default(), None),
            policy: arc_swap::ArcSwap::from_pointee(ResiliencePolicy::defaults()),
        }
    }

    /// Atomically replace the derived policy (retry, timeouts, fallbacks) from a
    /// new config — the hot-reload entry point. Circuit-breaker state is left
    /// untouched, so an open circuit stays open across a reload.
    pub fn reload_policy(&self, config: &Config) {
        self.policy
            .store(Arc::new(ResiliencePolicy::from_config(config)));
    }

    /// Mutate the current policy in place (builder helper for tests + overrides).
    fn map_policy(self, f: impl FnOnce(&mut ResiliencePolicy)) -> Self {
        let mut policy = (*self.policy.load_full()).clone();
        f(&mut policy);
        self.policy.store(Arc::new(policy));
        self
    }

    /// Override the default first-token timeout (builder style). Used by tests
    /// and by callers that drive the executor without a full config.
    #[must_use]
    pub fn with_first_token(self, first_token: Duration) -> Self {
        self.map_policy(|p| p.default_timeouts.first_token = first_token)
    }

    /// Override the retry policy (builder style) — e.g. tests that assert on a
    /// single attempt.
    #[must_use]
    pub fn with_retry(self, retry: RetryPolicy) -> Self {
        self.map_policy(|p| p.retry = retry)
    }

    /// The ordered chain of client-facing model ids to try for `model`: the
    /// model itself first, then its configured fallbacks.
    #[must_use]
    pub fn chain_ids(&self, model: &str) -> Vec<String> {
        let policy = self.policy.load();
        let mut ids = Vec::with_capacity(1 + policy.fallbacks.get(model).map_or(0, Vec::len));
        ids.push(model.to_owned());
        if let Some(fallbacks) = policy.fallbacks.get(model) {
            ids.extend(fallbacks.iter().cloned());
        }
        ids
    }

    /// The execution knobs (retry + timeouts) for `model`, applying the
    /// per-model timeout override when present.
    #[must_use]
    pub fn exec_config(&self, model: &str) -> ExecConfig {
        let policy = self.policy.load();
        let t = policy
            .model_timeouts
            .get(model)
            .copied()
            .unwrap_or(policy.default_timeouts);
        ExecConfig {
            retry: policy.retry,
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
