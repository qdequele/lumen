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
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::{HeaderMap, HeaderName, HeaderValue};
use lumen_core::{Capability, GatewayError};
use lumen_router::circuit::{BreakerConfig, CircuitBreakers};
use lumen_router::executor::{ExecConfig, Link};
use lumen_router::retry::RetryPolicy;
use lumen_router::virtual_models::{
    Decision, FactSource, FoundationIndex, Preset, RoutingTable, VirtualModelConfig,
};
use lumen_telemetry::ResilienceMetrics;

use crate::account_models::{AccountModels, ParsedRow};
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
    /// Resolved retry policy of each provider that overrides any retry knob
    /// (ADR 021), keyed by provider name; the rest use `retry`.
    provider_retry: HashMap<String, RetryPolicy>,
    default_timeouts: Timeouts,
    /// Per-model timeout overrides (inherited from the owning provider).
    model_timeouts: HashMap<String, Timeouts>,
    /// Compiled virtual models (ADR 014), swapped with the rest of the policy
    /// on reload.
    routing: Arc<RoutingTable>,
    /// The foundation models `routing` was compiled against; account tables
    /// (ADR 022) compile against the same snapshot.
    foundation: Arc<FoundationIndex>,
    /// Deprecated bare id -> its `provider/model` (or `namespace/model`) id,
    /// from the same config snapshot as `routing`. Hidden: never listed.
    aliases: Arc<HashMap<String, String>>,
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
        let retry = RetryPolicy {
            max_attempts: r.retry_max_attempts,
            base: Duration::from_millis(r.retry_base_ms),
            max: Duration::from_millis(r.retry_max_ms),
            max_retry_after: Duration::from_millis(r.retry_after_max_ms),
        };
        let provider_retry = config
            .providers
            .iter()
            .filter(|p| {
                p.retry_max_attempts.is_some()
                    || p.retry_base_ms.is_some()
                    || p.retry_max_ms.is_some()
                    || p.retry_after_max_ms.is_some()
            })
            .map(|p| {
                let millis =
                    |v: Option<u64>, default: Duration| v.map_or(default, Duration::from_millis);
                (
                    p.name.clone(),
                    RetryPolicy {
                        max_attempts: p.retry_max_attempts.unwrap_or(retry.max_attempts),
                        base: millis(p.retry_base_ms, retry.base),
                        max: millis(p.retry_max_ms, retry.max),
                        max_retry_after: millis(p.retry_after_max_ms, retry.max_retry_after),
                    },
                )
            })
            .collect();
        Self {
            retry,
            provider_retry,
            default_timeouts,
            model_timeouts,
            routing: Arc::new(routing),
            foundation: Arc::new(config.foundation_index()),
            aliases: Arc::new(config.model_aliases()),
        }
    }

    fn defaults() -> Self {
        Self {
            retry: RetryPolicy::default(),
            provider_retry: HashMap::new(),
            default_timeouts: Timeouts {
                first_token: Duration::from_secs(30),
                total: Duration::from_secs(600),
            },
            model_timeouts: HashMap::new(),
            routing: Arc::new(RoutingTable::default()),
            foundation: Arc::new(FoundationIndex::default()),
            aliases: Arc::new(HashMap::new()),
        }
    }
}

/// The circuit-breaker tuning of `config`.
fn breaker_config(config: &Config) -> BreakerConfig {
    BreakerConfig {
        failure_threshold: config.resilience.circuit_failure_threshold,
        cooldown: Duration::from_millis(config.resilience.circuit_cooldown_ms),
    }
}

/// Process-wide resilience state. The circuit breakers are stable for the life
/// of the process (so their state survives a hot reload); the derived policy is
/// behind an [`ArcSwap`] so a config reload can replace it atomically without
/// touching breaker state (DEBT-1 / M7 §7.3). The breaker tuning follows the
/// reload through [`CircuitBreakers::set_config`].
#[derive(Debug)]
pub struct ResilienceRuntime {
    /// Per-(provider, model) circuit breakers - never rebuilt on reload.
    pub breakers: CircuitBreakers,
    policy: arc_swap::ArcSwap<ResiliencePolicy>,
    /// Counts deprecated-alias requests (`lumen_deprecated_model_alias_total`).
    metrics: Option<ResilienceMetrics>,
    /// Each account's own virtual models (ADR 022), compiled over the
    /// platform policy; read lock-free on the request path.
    accounts: arc_swap::ArcSwap<HashMap<String, Arc<AccountModels>>>,
    /// Serialises the writers of `accounts` (admin writes, the refresher, a
    /// platform reload), so none of them loses another's update. Held only
    /// for an in-memory compile, never across an `.await`.
    accounts_write: Mutex<()>,
    /// The highest store generation installed into `accounts`.
    account_generation: AtomicI64,
}

impl ResilienceRuntime {
    /// Build from config, wiring circuit-state transitions to `metrics` when
    /// provided.
    #[must_use]
    pub fn from_config(config: &Config, metrics: Option<ResilienceMetrics>) -> Self {
        Self {
            breakers: CircuitBreakers::new(breaker_config(config), metrics.clone()),
            policy: arc_swap::ArcSwap::from_pointee(ResiliencePolicy::from_config(config)),
            metrics,
            accounts: arc_swap::ArcSwap::from_pointee(HashMap::new()),
            accounts_write: Mutex::new(()),
            account_generation: AtomicI64::new(0),
        }
    }

    /// A runtime with library defaults, no virtual models and no gauge - used by
    /// tests and as the open-gateway baseline.
    #[must_use]
    pub fn defaults() -> Self {
        Self {
            breakers: CircuitBreakers::new(BreakerConfig::default(), None),
            policy: arc_swap::ArcSwap::from_pointee(ResiliencePolicy::defaults()),
            metrics: None,
            accounts: arc_swap::ArcSwap::from_pointee(HashMap::new()),
            accounts_write: Mutex::new(()),
            account_generation: AtomicI64::new(0),
        }
    }

    /// Atomically replace the derived policy (retry, timeouts, virtual models) from a
    /// new config - the hot-reload entry point. Circuit-breaker state is left
    /// untouched, so an open circuit stays open across a reload; the breaker
    /// tuning (`circuit_failure_threshold`, `circuit_cooldown_ms`) is live,
    /// for existing breakers too.
    pub fn reload_policy(&self, config: &Config) {
        self.policy
            .store(Arc::new(ResiliencePolicy::from_config(config)));
        self.breakers.set_config(breaker_config(config));
        // Account tables hold compiled platform nodes: rebuild them over the
        // new platform snapshot (ADR 022).
        self.recompile_accounts();
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

    /// The id a request's `model` names: itself, or, for a deprecated bare id
    /// (`gpt-4o` for `openai/gpt-4o`), the prefixed id it aliases. An alias
    /// resolves but is never listed; each use is logged and counted on
    /// `lumen_deprecated_model_alias_total` so the aliases can be dropped once
    /// nobody sends them.
    #[must_use]
    pub fn canonical_model(&self, requested: &str) -> String {
        self.canonical_model_for(None, requested)
    }

    /// [`canonical_model`](Self::canonical_model) for a key of `account`:
    /// an id of the account's own virtual models (ADR 022) is never
    /// rewritten, so the account's model wins over a platform alias.
    #[must_use]
    pub fn canonical_model_for(&self, account: Option<&str>, requested: &str) -> String {
        if let Some(models) = account.and_then(|a| self.account_models(a)) {
            if models.table.get(requested).is_some() || models.table.is_disabled(requested) {
                return requested.to_owned();
            }
        }
        let policy = self.policy.load();
        let Some(canonical) = policy.aliases.get(requested) else {
            return requested.to_owned();
        };
        tracing::warn!(
            alias = %requested,
            model = %canonical,
            "deprecated bare model id: send the `provider/model` id instead"
        );
        if let Some(metrics) = &self.metrics {
            metrics.inc_deprecated_alias(requested, canonical);
        }
        canonical.clone()
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
        self.decide_for(capability, None, model, facts)
    }

    /// [`decide`](Self::decide) for a key of `account` (ADR 022): the
    /// account's own virtual models are looked up first, then the platform
    /// as before. A key without an account sees the platform only. One map
    /// lookup more than `decide`, no I/O.
    ///
    /// # Errors
    /// `LM-2002` when `model` is a virtual model serving another capability;
    /// `LM-2001` for a disabled account model, or one whose every target
    /// reached by this request was skipped.
    pub fn decide_for<F: FactSource>(
        &self,
        capability: Capability,
        account: Option<&str>,
        model: &str,
        facts: impl FnOnce(Option<&Preset>) -> F,
    ) -> Result<Decision, GatewayError> {
        use rand::Rng as _;
        let mut draw = || rand::rng().next_u64();
        if let Some(models) = account.and_then(|a| self.account_models(a)) {
            if let Some(vm) = models.table.get(model) {
                let facts = facts(vm.preset().map(AsRef::as_ref));
                return vm.decide(capability, &facts, &mut draw);
            }
            if models.table.is_disabled(model) {
                return Err(GatewayError::ModelNotFound(model.to_owned()));
            }
        }
        let policy = self.policy.load();
        let Some(vm) = policy.routing.get(model) else {
            return Ok(Decision::direct(model));
        };
        let facts = facts(vm.preset().map(AsRef::as_ref));
        vm.decide(capability, &facts, &mut draw)
    }

    /// One account's virtual models (ADR 022), when it has any stored.
    #[must_use]
    pub fn account_models(&self, account: &str) -> Option<Arc<AccountModels>> {
        self.accounts.load().get(account).cloned()
    }

    /// The store generation the refresher has read up to (it polls past it).
    #[must_use]
    pub fn account_generation(&self) -> i64 {
        self.account_generation.load(Ordering::SeqCst)
    }

    /// Move the refresher's watermark to `generation` (never backwards).
    pub fn advance_account_generation(&self, generation: i64) {
        self.account_generation
            .fetch_max(generation, Ordering::SeqCst);
    }

    /// Compile `configs` as `account`'s table over the current platform
    /// snapshot, without installing it: the admin write path checks the
    /// result before storing.
    #[must_use]
    pub fn compile_account(&self, configs: &[VirtualModelConfig]) -> RoutingTable {
        let policy = self.policy.load();
        RoutingTable::compile_account(configs, &policy.foundation, &policy.routing)
    }

    /// Whether `id` is a platform id: a foundation model (disabled ones
    /// included), a platform virtual model or a deprecated bare alias. An
    /// account may not save a model under one (ADR 022).
    #[must_use]
    pub fn is_platform_id(&self, id: &str) -> bool {
        let policy = self.policy.load();
        policy.foundation.contains(id)
            || policy.routing.get(id).is_some()
            || policy.aliases.contains_key(id)
    }

    /// Compile and install stored account documents. A row older than what
    /// is installed for its account (a refresher racing an admin write) is
    /// ignored. Only the refresher passes `advance_watermark`: an admin
    /// write installs its own row without moving the watermark, or the
    /// refresher would skip rows other processes wrote before it.
    pub fn install_accounts(&self, rows: Vec<ParsedRow>, advance_watermark: bool) {
        let _guard = self
            .accounts_write
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let policy = self.policy.load();
        let mut map = HashMap::clone(&self.accounts.load());
        for (account, configs, hash, generation) in rows {
            if map.get(&account).is_some_and(|m| m.generation > generation) {
                continue;
            }
            let table =
                RoutingTable::compile_account(&configs, &policy.foundation, &policy.routing);
            if advance_watermark {
                self.advance_account_generation(generation);
            }
            map.insert(
                account,
                Arc::new(AccountModels {
                    configs,
                    hash,
                    generation,
                    table,
                }),
            );
        }
        self.accounts.store(Arc::new(map));
    }

    /// Recompile every account table over the current platform snapshot
    /// (after a platform reload).
    pub fn recompile_accounts(&self) {
        let _guard = self
            .accounts_write
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let current = self.accounts.load_full();
        if current.is_empty() {
            return;
        }
        let policy = self.policy.load();
        let map = current
            .iter()
            .map(|(account, models)| {
                let table = RoutingTable::compile_account(
                    &models.configs,
                    &policy.foundation,
                    &policy.routing,
                );
                (
                    account.clone(),
                    Arc::new(AccountModels {
                        configs: models.configs.clone(),
                        hash: models.hash.clone(),
                        generation: models.generation,
                        table,
                    }),
                )
            })
            .collect();
        self.accounts.store(Arc::new(map));
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

    /// Give each link its provider's own knobs: the retry policy when that
    /// provider overrides one (ADR 021; other links keep
    /// [`ExecConfig::retry`]) and the first-token timeout (ADR 005, per-link
    /// amendment), so a fallback is bounded by its own provider's
    /// `first_token_timeout_ms` and not by the primary's.
    /// Call once the chain's links are built, before executing.
    pub fn apply_retry_overrides(&self, links: &mut [Link]) {
        let policy = self.policy.load();
        if !policy.provider_retry.is_empty() {
            for link in links.iter_mut() {
                link.retry = policy.provider_retry.get(&link.provider_name).copied();
            }
        }
        // Without any override every link shares `ExecConfig::first_token`
        // (the default): nothing to carry.
        if !policy.model_timeouts.is_empty() {
            for link in links {
                link.first_token = Some(
                    policy
                        .model_timeouts
                        .get(&link.model_id)
                        .map_or(policy.default_timeouts.first_token, |t| t.first_token),
                );
            }
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
            id = "slow/gpt"
            capabilities = ["chat"]
            [[providers]]
            name = "fast"
            kind = "anthropic"
            [[providers.models]]
            id = "fast/claude"
            capabilities = ["chat"]
        "#,
        );
        let rt = ResilienceRuntime::from_config(&cfg, None);
        // Overridden first-token, default total.
        let slow = rt.exec_config("slow/gpt");
        assert_eq!(slow.first_token, Duration::from_secs(90));
        assert_eq!(slow.total, Duration::from_secs(600));
        // No override → global default.
        let fast = rt.exec_config("fast/claude");
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
            id = "a/gpt"
            capabilities = ["chat"]
            [[providers.models]]
            id = "a/claude"
            capabilities = ["chat"]

            [[virtual_models]]
            id = "lumen/v"
            capability = "chat"
            strategy = "fallback"
            targets = [{ model = "a/claude" }, { model = "a/gpt" }]

            [[virtual_models]]
            id = "lumen/p"
            capability = "chat"
            strategy = "single"
            preset = { system_prompt = "be brief" }
            targets = [{ model = "a/gpt" }]
            "#,
        )
        .unwrap();
        let rt = ResilienceRuntime::from_config(&cfg, None);
        let mut saw_preset = None;
        let d = rt
            .decide(Capability::Chat, "lumen/v", |preset| {
                saw_preset = Some(preset.is_some());
                NoFacts
            })
            .unwrap();
        assert_eq!(d.virtual_model.as_deref(), Some("lumen/v"));
        assert_eq!(d.primary_model(), "a/claude");
        assert_eq!(
            saw_preset,
            Some(false),
            "facts are built for a virtual model"
        );
        // The preset handed to the facts builder comes from the same snapshot.
        let mut saw_preset = false;
        rt.decide(Capability::Chat, "lumen/p", |preset| {
            saw_preset = preset.is_some();
            NoFacts
        })
        .unwrap();
        assert!(saw_preset);
        // A foundation id is a direct single attempt, with no virtual model,
        // and its facts are never built.
        let direct = rt
            .decide(Capability::Chat, "a/gpt", |_| -> NoFacts {
                panic!("facts built for a foundation id")
            })
            .unwrap();
        assert!(direct.virtual_model.is_none());
        let ids: Vec<&str> = direct
            .attempts
            .iter()
            .map(|a| a.model_id.as_str())
            .collect();
        assert_eq!(ids, vec!["a/gpt"]);
        assert!(rt.routing().get("lumen/v").is_some());
    }

    #[test]
    fn canonical_model_resolves_a_bare_alias_and_counts_it() {
        let cfg: Config = toml::from_str(
            r#"
            [[providers]]
            name = "openai"
            kind = "openai"
            [[providers.models]]
            id = "openai/gpt-4o"
            capabilities = ["chat"]
            "#,
        )
        .unwrap();
        let metrics = lumen_telemetry::Metrics::new();
        let gauges = ResilienceMetrics::register(&metrics).unwrap();
        let rt = ResilienceRuntime::from_config(&cfg, Some(gauges));
        assert_eq!(rt.canonical_model("openai/gpt-4o"), "openai/gpt-4o");
        assert_eq!(rt.canonical_model("unknown"), "unknown");
        assert!(!metrics
            .encode_text()
            .contains("lumen_deprecated_model_alias_total{"));
        assert_eq!(rt.canonical_model("gpt-4o"), "openai/gpt-4o");
        assert!(metrics.encode_text().contains(
            "lumen_deprecated_model_alias_total{alias=\"gpt-4o\",model=\"openai/gpt-4o\"} 1"
        ));
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

    #[test]
    fn provider_retry_overrides_land_on_their_links_only() {
        let cfg = load(
            r#"
            [resilience]
            retry_max_attempts = 2
            retry_base_ms = 100
            retry_max_ms = 1000
            retry_after_max_ms = 30000
            [[providers]]
            name = "hf"
            kind = "tei"
            base_url = "http://127.0.0.1:9"
            retry_max_attempts = 6
            retry_base_ms = 1000
            [[providers.models]]
            id = "hf/bge-m3"
            capabilities = ["embed"]
            [[providers]]
            name = "oa"
            kind = "openai"
            [[providers.models]]
            id = "oa/embed"
            capabilities = ["embed"]
        "#,
        );
        let rt = ResilienceRuntime::from_config(&cfg, None);
        let link = |provider: &str| Link {
            provider_name: provider.to_owned(),
            model_id: format!("{provider}/m"),
            escapes: Vec::new(),
            breaker_scope: None,
            retry: None,
            first_token: None,
        };
        let mut links = vec![link("hf"), link("oa")];
        rt.apply_retry_overrides(&mut links);
        // Overridden fields from the provider, the rest inherited.
        assert_eq!(
            links[0].retry,
            Some(RetryPolicy {
                max_attempts: 6,
                base: Duration::from_millis(1000),
                max: Duration::from_millis(1000),
                max_retry_after: Duration::from_secs(30),
            })
        );
        assert_eq!(links[1].retry, None, "no override: the global policy");
        let global = rt.exec_config("oa/embed").retry;
        assert_eq!(global.max_attempts, 2);
        assert_eq!(global.max_retry_after, Duration::from_secs(30));
    }

    #[test]
    fn breaker_tuning_follows_hot_reload_and_keeps_state() {
        let toml = |threshold: u32, cooldown_ms: u64| {
            format!(
                r#"
                [resilience]
                circuit_failure_threshold = {threshold}
                circuit_cooldown_ms = {cooldown_ms}
                [[providers]]
                name = "oa"
                kind = "openai"
                [[providers.models]]
                id = "oa/gpt"
                capabilities = ["chat"]
                "#
            )
        };
        let rt = ResilienceRuntime::from_config(&load(&toml(5, 30_000)), None);
        let breaker = rt.breakers.get("oa", "oa/gpt");
        let now = tokio::time::Instant::now();
        breaker.on_failure(now);
        breaker.on_failure(now);

        rt.reload_policy(&load(&toml(3, 1_000)));
        assert_eq!(
            rt.breakers.config(),
            BreakerConfig {
                failure_threshold: 3,
                cooldown: Duration::from_secs(1),
            }
        );
        // The breaker that existed before the reload: its streak of two is
        // kept and the new threshold of three applies to it.
        breaker.on_failure(now);
        assert_eq!(
            breaker.state(),
            lumen_router::circuit::CircuitState::Open,
            "an existing breaker reads the reloaded threshold"
        );
        assert!(matches!(
            breaker.admit(now + Duration::from_secs(2)),
            lumen_router::circuit::Admission::Probe(_)
        ));
    }

    #[test]
    fn each_link_gets_its_own_provider_first_token_timeout() {
        let cfg = load(
            r#"
            [server]
            first_token_timeout_ms = 30000
            [[providers]]
            name = "oa"
            kind = "openai"
            [[providers.models]]
            id = "oa/gpt"
            capabilities = ["chat"]
            [[providers]]
            name = "ollama"
            kind = "ollama"
            base_url = "http://127.0.0.1:9"
            first_token_timeout_ms = 60000
            [[providers.models]]
            id = "ollama/llama"
            capabilities = ["chat"]
        "#,
        );
        let rt = ResilienceRuntime::from_config(&cfg, None);
        let link = |provider: &str, model: &str| Link {
            provider_name: provider.to_owned(),
            model_id: model.to_owned(),
            escapes: Vec::new(),
            breaker_scope: None,
            retry: None,
            first_token: None,
        };
        // The override applies to the fallback although the chain's
        // ExecConfig is resolved from the primary.
        let mut links = vec![link("oa", "oa/gpt"), link("ollama", "ollama/llama")];
        rt.apply_retry_overrides(&mut links);
        assert_eq!(
            rt.exec_config("oa/gpt").first_token,
            Duration::from_secs(30)
        );
        assert_eq!(links[0].first_token, Some(Duration::from_secs(30)));
        assert_eq!(links[1].first_token, Some(Duration::from_secs(60)));
        // And the other way round: a fallback without an override does not
        // inherit the primary's longer deadline.
        let mut links = vec![link("ollama", "ollama/llama"), link("oa", "oa/gpt")];
        rt.apply_retry_overrides(&mut links);
        assert_eq!(links[0].first_token, Some(Duration::from_secs(60)));
        assert_eq!(links[1].first_token, Some(Duration::from_secs(30)));
    }

    #[test]
    fn provider_retry_overrides_follow_hot_reload() {
        let toml = |attempts: u32| {
            format!(
                r#"
                [[providers]]
                name = "hf"
                kind = "tei"
                base_url = "http://127.0.0.1:9"
                retry_max_attempts = {attempts}
                [[providers.models]]
                id = "hf/bge-m3"
                capabilities = ["embed"]
                "#
            )
        };
        let rt = ResilienceRuntime::from_config(&load(&toml(4)), None);
        let mut links = vec![Link {
            provider_name: "hf".to_owned(),
            model_id: "hf/bge-m3".to_owned(),
            escapes: Vec::new(),
            breaker_scope: None,
            retry: None,
            first_token: None,
        }];
        rt.apply_retry_overrides(&mut links);
        assert_eq!(links[0].retry.map(|r| r.max_attempts), Some(4));

        rt.reload_policy(&load(&toml(7)));
        rt.apply_retry_overrides(&mut links);
        assert_eq!(links[0].retry.map(|r| r.max_attempts), Some(7));
    }
}
