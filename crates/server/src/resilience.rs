//! Server-side resilience runtime (M6): the process-wide circuit breakers plus
//! the resolved retry policy, timeouts and compiled virtual models (ADR 014)
//! derived from config.
//!
//! This is the glue between [`Config`](crate::config::Config) and the router's
//! [`executor`](lumen_router::executor): the handlers ask it for a request's
//! attempts ([`decide`](ResilienceRuntime::decide)) and the per-model
//! execution knobs ([`exec_config`](ResilienceRuntime::exec_config)).
//! All state is in-memory; nothing here touches a database.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::http::{HeaderMap, HeaderName, HeaderValue};
use lumen_core::{Capability, GatewayError};
use lumen_router::circuit::{BreakerConfig, CircuitBreakers};
use lumen_router::executor::ExecConfig;
use lumen_router::retry::RetryPolicy;
use lumen_router::virtual_models::{Decision, FactSource, Preset, RoutingTable};
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

/// The `x-lumen-route` response header name (ADR 014).
const ROUTE_HEADER: &str = "x-lumen-route";

/// [`model_used_headers`] plus, for a virtual-model request, the route it
/// took (`x-lumen-route`, e.g. `acme/chat>acme/eu>mistral-large`).
#[must_use]
pub fn routing_headers(model_used: &str, route: Option<&str>) -> HeaderMap {
    let mut headers = model_used_headers(model_used);
    if let Some(value) = route.and_then(|r| HeaderValue::from_str(r).ok()) {
        headers.insert(HeaderName::from_static(ROUTE_HEADER), value);
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
/// config file. The circuit breakers are deliberately *not* here - their live
/// state must survive a reload (a reload must not reset an open circuit).
#[derive(Debug, Clone)]
struct ResiliencePolicy {
    retry: RetryPolicy,
    default_timeouts: Timeouts,
    /// Per-model timeout overrides (inherited from the owning provider).
    model_timeouts: HashMap<String, Timeouts>,
    /// Compiled virtual models (ADR 014), swapped with the rest of the policy
    /// on reload.
    routing: Arc<RoutingTable>,
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
        let routing = match config.routing_table() {
            Ok((table, warnings)) => {
                for warning in warnings {
                    tracing::warn!(%warning, "virtual models");
                }
                table
            }
            // Unreachable for a validated config; never fail a reload here.
            Err(error) => {
                tracing::error!(%error, "virtual models failed to compile after validation; serving without them");
                RoutingTable::default()
            }
        };
        Self {
            retry: RetryPolicy {
                max_attempts: r.retry_max_attempts,
                base: Duration::from_millis(r.retry_base_ms),
                max: Duration::from_millis(r.retry_max_ms),
            },
            default_timeouts,
            model_timeouts,
            routing: Arc::new(routing),
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
            routing: Arc::new(RoutingTable::default()),
        }
    }
}

/// Process-wide resilience state. The circuit breakers are stable for the life
/// of the process (so their state survives a hot reload); the derived policy is
/// behind an [`ArcSwap`] so a config reload can replace it atomically without
/// touching breaker state (DEBT-1 / M7 §7.3).
#[derive(Debug)]
pub struct ResilienceRuntime {
    /// Per-(provider, model) circuit breakers - never rebuilt on reload.
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

    /// A runtime with library defaults, no virtual models and no gauge - used by
    /// tests and as the open-gateway baseline.
    #[must_use]
    pub fn defaults() -> Self {
        Self {
            breakers: CircuitBreakers::new(BreakerConfig::default(), None),
            policy: arc_swap::ArcSwap::from_pointee(ResiliencePolicy::defaults()),
        }
    }

    /// Atomically replace the derived policy (retry, timeouts, virtual models) from a
    /// new config - the hot-reload entry point. Circuit-breaker state is left
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

    /// Override the retry policy (builder style) - e.g. tests that assert on a
    /// single attempt.
    #[must_use]
    pub fn with_retry(self, retry: RetryPolicy) -> Self {
        self.map_policy(|p| p.retry = retry)
    }

    /// Decide the attempts for `model` (ADR 014) from ONE routing snapshot: a
    /// virtual model's routing tree or a direct single attempt on a foundation
    /// model. Pure and in-memory: one hash lookup, one random draw per `split`.
    ///
    /// `facts` builds the request facts `switch` conditions read. It is only
    /// called for a virtual model (a foundation id builds no facts), and it
    /// receives that virtual model's preset from the same snapshot the decision
    /// comes from, so a chat caller applies the preset prompt before the facts
    /// are computed (its prompt counts toward `input_tokens`) and the preset
    /// and the attempts can never come from two different reloads.
    ///
    /// # Errors
    /// `LM-2002` when `model` is a virtual model serving another capability.
    pub fn decide<F: FactSource>(
        &self,
        capability: Capability,
        model: &str,
        facts: impl FnOnce(Option<&Preset>) -> F,
    ) -> Result<Decision, GatewayError> {
        use rand::Rng as _;
        let policy = self.policy.load();
        let Some(vm) = policy.routing.get(model) else {
            return Ok(Decision::direct(model));
        };
        let facts = facts(vm.preset().map(AsRef::as_ref));
        let mut draw = || rand::rng().next_u64();
        vm.decide(capability, &facts, &mut draw)
    }

    /// The current compiled virtual models (for `GET /v1/models` and the
    /// admin plan route).
    #[must_use]
    pub fn routing(&self) -> Arc<RoutingTable> {
        self.policy.load().routing.clone()
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

    struct NoFacts;
    impl lumen_router::virtual_models::FactSource for NoFacts {
        fn group(&self) -> Option<&str> {
            None
        }
        fn metadata(&self, _: &str) -> Option<&serde_json::Value> {
            None
        }
        fn has_images(&self) -> bool {
            false
        }
        fn has_tools(&self) -> bool {
            false
        }
        fn stream(&self) -> bool {
            false
        }
        fn input_tokens(&self) -> u64 {
            0
        }
        fn documents(&self) -> Option<u64> {
            None
        }
    }

    #[test]
    fn decide_routes_virtual_models_and_direct_ids() {
        let cfg: Config = toml::from_str(
            r#"
            [[providers]]
            name = "a"
            kind = "openai"
            [[providers.models]]
            id = "gpt"
            capabilities = ["chat"]
            [[providers.models]]
            id = "claude"
            capabilities = ["chat"]

            [[virtual_models]]
            id = "v"
            capability = "chat"
            strategy = "fallback"
            targets = [{ model = "claude" }, { model = "gpt" }]

            [[virtual_models]]
            id = "p"
            capability = "chat"
            strategy = "single"
            preset = { system_prompt = "be brief" }
            targets = [{ model = "gpt" }]
            "#,
        )
        .unwrap();
        let rt = ResilienceRuntime::from_config(&cfg, None);
        let mut saw_preset = None;
        let d = rt
            .decide(Capability::Chat, "v", |preset| {
                saw_preset = Some(preset.is_some());
                NoFacts
            })
            .unwrap();
        assert_eq!(d.virtual_model.as_deref(), Some("v"));
        assert_eq!(d.primary_model(), "claude");
        assert_eq!(
            saw_preset,
            Some(false),
            "facts are built for a virtual model"
        );
        // The preset handed to the facts builder comes from the same snapshot.
        let mut saw_preset = false;
        rt.decide(Capability::Chat, "p", |preset| {
            saw_preset = preset.is_some();
            NoFacts
        })
        .unwrap();
        assert!(saw_preset);
        // A foundation id is a direct single attempt, with no virtual model,
        // and its facts are never built.
        let direct = rt
            .decide(Capability::Chat, "gpt", |_| -> NoFacts {
                panic!("facts built for a foundation id")
            })
            .unwrap();
        assert!(direct.virtual_model.is_none());
        let ids: Vec<&str> = direct
            .attempts
            .iter()
            .map(|a| a.model_id.as_str())
            .collect();
        assert_eq!(ids, vec!["gpt"]);
        assert!(rt.routing().get("v").is_some());
    }

    #[test]
    fn routing_headers_carry_the_route_only_for_virtual_models() {
        let h = routing_headers("gpt-4o", Some("acme/chat>gpt-4o"));
        assert_eq!(h.get("x-lumen-model-used").unwrap(), "gpt-4o");
        assert_eq!(h.get("x-lumen-route").unwrap(), "acme/chat>gpt-4o");
        assert!(routing_headers("gpt-4o", None)
            .get("x-lumen-route")
            .is_none());
    }
}
