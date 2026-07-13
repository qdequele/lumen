//! Per-(provider, model) circuit breaker (M6 §6.3).
//!
//! A breaker trips Closed → Open after `failure_threshold` consecutive
//! provider-fault failures, stays Open for `cooldown`, then goes Half-Open and
//! admits exactly **one** probe: success closes it, failure reopens it. State
//! is pure in-memory and never touches a database or an `.await` while locked,
//! so it is safe on the request hot path (pillar 1). Every transition is pushed
//! to the `lumen_circuit_state` gauge (M6 §6.3) when a
//! [`ResilienceMetrics`] handle is supplied.
//!
//! Time is passed in as a [`tokio::time::Instant`] so the state machine is
//! deterministic under `tokio::time` pause in tests.

use std::sync::Mutex;

use dashmap::DashMap;
use lumen_telemetry::resilience::{
    ResilienceMetrics, CIRCUIT_CLOSED, CIRCUIT_HALF_OPEN, CIRCUIT_OPEN,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

/// Breaker tuning. Defaults (M6 spec): open after 5 failures, 30 s cooldown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BreakerConfig {
    /// Consecutive provider-fault failures that trip the breaker open.
    pub failure_threshold: u32,
    /// How long the breaker stays open before admitting a half-open probe.
    pub cooldown: Duration,
}

impl Default for BreakerConfig {
    fn default() -> Self {
        Self {
            failure_threshold: 5,
            cooldown: Duration::from_secs(30),
        }
    }
}

/// Observable breaker state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitState {
    /// Calls flow normally.
    Closed,
    /// Calls are short-circuited until the cooldown elapses.
    Open,
    /// One probe is being admitted to test recovery.
    HalfOpen,
}

impl CircuitState {
    const fn gauge_value(self) -> i64 {
        match self {
            CircuitState::Closed => CIRCUIT_CLOSED,
            CircuitState::Open => CIRCUIT_OPEN,
            CircuitState::HalfOpen => CIRCUIT_HALF_OPEN,
        }
    }
}

/// The result of asking a breaker for permission to call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// Proceed (Closed, or the single Half-Open probe).
    Allowed,
    /// Short-circuit; advertise roughly how long until a probe is admitted.
    Rejected { retry_after: Duration },
}

#[derive(Debug)]
struct Inner {
    state: CircuitState,
    consecutive_failures: u32,
    opened_at: Option<Instant>,
    /// Half-Open: a probe is in flight, so no other caller may probe.
    probe_in_flight: bool,
    /// When the outstanding half-open probe was admitted. Used to auto-rearm:
    /// a probe whose result never comes back (client disconnect, total-timeout,
    /// or a non-provider-fault error that neither closes nor reopens the
    /// breaker) would otherwise pin `probe_in_flight = true` forever. After a
    /// cooldown with no resolution the probe is presumed lost and a fresh one
    /// is admitted, so the breaker can never wedge shut.
    probe_admitted_at: Option<Instant>,
}

/// One circuit breaker, keyed externally by (provider, model).
#[derive(Debug)]
pub struct CircuitBreaker {
    inner: Mutex<Inner>,
    config: BreakerConfig,
    provider: String,
    model: String,
    metrics: Option<ResilienceMetrics>,
}

impl CircuitBreaker {
    fn new(
        provider: String,
        model: String,
        config: BreakerConfig,
        metrics: Option<ResilienceMetrics>,
    ) -> Self {
        Self {
            inner: Mutex::new(Inner {
                state: CircuitState::Closed,
                consecutive_failures: 0,
                opened_at: None,
                probe_in_flight: false,
                probe_admitted_at: None,
            }),
            config,
            provider,
            model,
            metrics,
        }
    }

    /// Lock, recovering from a poisoned mutex rather than panicking (a panic in
    /// the request path is forbidden — CLAUDE.md rule 1). The guarded data is
    /// plain counters, so an inconsistent state after a panic is harmless.
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn report(&self, state: CircuitState) {
        if let Some(metrics) = &self.metrics {
            metrics.set_circuit_state(&self.provider, &self.model, state.gauge_value());
        }
    }

    /// Ask permission to call at `now`. Advances Open → Half-Open when the
    /// cooldown has elapsed, handing the caller the sole probe.
    pub fn admit(&self, now: Instant) -> Admission {
        let mut inner = self.lock();
        match inner.state {
            CircuitState::Closed => Admission::Allowed,
            CircuitState::Open => {
                let ready_at = inner
                    .opened_at
                    .map_or(now, |opened| opened + self.config.cooldown);
                if now >= ready_at {
                    inner.state = CircuitState::HalfOpen;
                    inner.probe_in_flight = true;
                    inner.probe_admitted_at = Some(now);
                    drop(inner);
                    self.report(CircuitState::HalfOpen);
                    Admission::Allowed
                } else {
                    Admission::Rejected {
                        retry_after: ready_at.saturating_duration_since(now),
                    }
                }
            }
            CircuitState::HalfOpen => {
                // A probe already in flight blocks others — unless it has been
                // outstanding longer than the cooldown, in which case it is
                // presumed lost (no on_success/on_failure ever ran) and a fresh
                // probe is admitted so the breaker cannot wedge shut.
                let expires_at = inner.probe_admitted_at.map(|t| t + self.config.cooldown);
                // MSRV 1.80: `Option::is_none_or` is 1.82, so use `map_or`.
                let stale = expires_at.map_or(true, |deadline| now >= deadline);
                if inner.probe_in_flight && !stale {
                    Admission::Rejected {
                        retry_after: expires_at
                            .map_or(self.config.cooldown, |d| d.saturating_duration_since(now)),
                    }
                } else {
                    inner.probe_in_flight = true;
                    inner.probe_admitted_at = Some(now);
                    Admission::Allowed
                }
            }
        }
    }

    /// Record a successful call: the provider is healthy again.
    pub fn on_success(&self) {
        let mut inner = self.lock();
        let was = inner.state;
        inner.state = CircuitState::Closed;
        inner.consecutive_failures = 0;
        inner.opened_at = None;
        inner.probe_in_flight = false;
        inner.probe_admitted_at = None;
        drop(inner);
        if was != CircuitState::Closed {
            self.report(CircuitState::Closed);
        }
    }

    /// Record a provider-fault failure at `now`. Trips the breaker open once
    /// the threshold is reached, or immediately if a half-open probe failed.
    pub fn on_failure(&self, now: Instant) {
        let mut inner = self.lock();
        match inner.state {
            CircuitState::HalfOpen => {
                // The probe failed: back to Open for another cooldown.
                inner.state = CircuitState::Open;
                inner.opened_at = Some(now);
                inner.probe_in_flight = false;
                inner.probe_admitted_at = None;
                drop(inner);
                self.report(CircuitState::Open);
            }
            CircuitState::Closed => {
                inner.consecutive_failures += 1;
                if inner.consecutive_failures >= self.config.failure_threshold {
                    inner.state = CircuitState::Open;
                    inner.opened_at = Some(now);
                    drop(inner);
                    self.report(CircuitState::Open);
                }
            }
            CircuitState::Open => {}
        }
    }

    /// The current state (for snapshots / observability).
    pub fn state(&self) -> CircuitState {
        self.lock().state
    }
}

/// The process-wide set of breakers, created lazily per (provider, model).
#[derive(Debug)]
pub struct CircuitBreakers {
    map: DashMap<(String, String), Arc<CircuitBreaker>>,
    config: BreakerConfig,
    metrics: Option<ResilienceMetrics>,
}

impl CircuitBreakers {
    /// Build an empty registry with the given tuning and optional gauge sink.
    #[must_use]
    pub fn new(config: BreakerConfig, metrics: Option<ResilienceMetrics>) -> Self {
        Self {
            map: DashMap::new(),
            config,
            metrics,
        }
    }

    /// The breaker for a (provider, model), created on first use.
    #[must_use]
    pub fn get(&self, provider: &str, model: &str) -> Arc<CircuitBreaker> {
        let key = (provider.to_owned(), model.to_owned());
        if let Some(existing) = self.map.get(&key) {
            return Arc::clone(existing.value());
        }
        let breaker = Arc::new(CircuitBreaker::new(
            provider.to_owned(),
            model.to_owned(),
            self.config,
            self.metrics.clone(),
        ));
        // `entry` keeps creation atomic against a racing insert of the same key.
        Arc::clone(self.map.entry(key).or_insert(breaker).value())
    }

    /// A snapshot of every known breaker's state (observability; not the hot
    /// path).
    #[must_use]
    pub fn snapshot(&self) -> Vec<(String, String, CircuitState)> {
        self.map
            .iter()
            .map(|e| {
                let (provider, model) = e.key();
                (provider.clone(), model.clone(), e.value().state())
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> BreakerConfig {
        BreakerConfig {
            failure_threshold: 5,
            cooldown: Duration::from_secs(30),
        }
    }

    fn breaker() -> CircuitBreaker {
        CircuitBreaker::new("openai".to_owned(), "gpt-4o".to_owned(), config(), None)
    }

    #[tokio::test]
    async fn opens_after_threshold_consecutive_failures() {
        let b = breaker();
        let now = Instant::now();
        for _ in 0..4 {
            assert_eq!(b.admit(now), Admission::Allowed);
            b.on_failure(now);
            assert_eq!(b.state(), CircuitState::Closed);
        }
        b.on_failure(now); // 5th
        assert_eq!(b.state(), CircuitState::Open);
        // A subsequent admit is rejected before the cooldown elapses.
        assert!(matches!(b.admit(now), Admission::Rejected { .. }));
    }

    #[tokio::test]
    async fn a_success_resets_the_failure_streak() {
        let b = breaker();
        let now = Instant::now();
        for _ in 0..4 {
            b.on_failure(now);
        }
        b.on_success();
        // Streak reset: four more failures still don't open it.
        for _ in 0..4 {
            b.on_failure(now);
        }
        assert_eq!(b.state(), CircuitState::Closed);
    }

    #[tokio::test]
    async fn open_transitions_to_half_open_after_cooldown_with_one_probe() {
        let b = breaker();
        let t0 = Instant::now();
        for _ in 0..5 {
            b.on_failure(t0);
        }
        assert_eq!(b.state(), CircuitState::Open);

        // Just before the cooldown: still rejected.
        let almost = t0 + Duration::from_secs(29);
        assert!(matches!(b.admit(almost), Admission::Rejected { .. }));

        // After the cooldown: exactly one probe is admitted, the next rejected.
        let after = t0 + Duration::from_secs(31);
        assert_eq!(b.admit(after), Admission::Allowed);
        assert_eq!(b.state(), CircuitState::HalfOpen);
        assert!(matches!(b.admit(after), Admission::Rejected { .. }));
    }

    #[tokio::test]
    async fn probe_success_closes_the_circuit() {
        let b = breaker();
        let t0 = Instant::now();
        for _ in 0..5 {
            b.on_failure(t0);
        }
        let after = t0 + Duration::from_secs(31);
        assert_eq!(b.admit(after), Admission::Allowed);
        b.on_success();
        assert_eq!(b.state(), CircuitState::Closed);
        assert_eq!(b.admit(after), Admission::Allowed);
    }

    #[tokio::test]
    async fn a_lost_probe_does_not_wedge_the_breaker_shut() {
        // The probe is admitted but its result never comes back (client
        // disconnect / total-timeout / non-fault error → neither on_success nor
        // on_failure runs). Without auto-rearm the breaker would reject forever.
        let b = breaker();
        let t0 = Instant::now();
        for _ in 0..5 {
            b.on_failure(t0);
        }
        let probe_at = t0 + Duration::from_secs(31);
        assert_eq!(b.admit(probe_at), Admission::Allowed); // probe admitted…
                                                           // …and simply never resolved. A second probe within the cooldown is
                                                           // still rejected (single-probe guarantee holds inside the window).
        assert!(matches!(
            b.admit(probe_at + Duration::from_secs(1)),
            Admission::Rejected { .. }
        ));
        // But once the lost probe's own cooldown elapses, a fresh probe passes.
        assert_eq!(
            b.admit(probe_at + Duration::from_secs(31)),
            Admission::Allowed,
            "a lost probe must not pin the breaker shut"
        );
    }

    #[tokio::test]
    async fn probe_failure_reopens_the_circuit() {
        let b = breaker();
        let t0 = Instant::now();
        for _ in 0..5 {
            b.on_failure(t0);
        }
        let after = t0 + Duration::from_secs(31);
        assert_eq!(b.admit(after), Admission::Allowed); // probe
        b.on_failure(after); // probe fails
        assert_eq!(b.state(), CircuitState::Open);
        // The cooldown restarts from the probe failure.
        assert!(matches!(
            b.admit(after + Duration::from_secs(1)),
            Admission::Rejected { .. }
        ));
        assert_eq!(b.admit(after + Duration::from_secs(31)), Admission::Allowed);
    }

    #[tokio::test]
    async fn rejected_advertises_a_positive_retry_after() {
        let b = breaker();
        let t0 = Instant::now();
        for _ in 0..5 {
            b.on_failure(t0);
        }
        match b.admit(t0 + Duration::from_secs(10)) {
            Admission::Rejected { retry_after } => {
                assert!(retry_after <= Duration::from_secs(20));
                assert!(retry_after > Duration::from_secs(0));
            }
            Admission::Allowed => panic!("should be rejected mid-cooldown"),
        }
    }

    #[tokio::test]
    async fn registry_returns_the_same_breaker_per_key() {
        let breakers = CircuitBreakers::new(config(), None);
        let a = breakers.get("openai", "gpt-4o");
        let now = Instant::now();
        for _ in 0..5 {
            a.on_failure(now);
        }
        // Same key → same breaker instance → sees the open state.
        let b = breakers.get("openai", "gpt-4o");
        assert_eq!(b.state(), CircuitState::Open);
        // Different model → independent breaker.
        let c = breakers.get("openai", "gpt-4o-mini");
        assert_eq!(c.state(), CircuitState::Closed);
        assert_eq!(breakers.snapshot().len(), 2);
    }
}
