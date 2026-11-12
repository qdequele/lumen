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

use std::borrow::Borrow;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
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

/// The tuning every breaker of a registry reads on each `admit` /
/// `on_failure`: two atomics, so a hot reload
/// ([`CircuitBreakers::set_config`]) reaches the existing breakers without a
/// lock on the request path and without touching their state.
#[derive(Debug)]
struct LiveConfig {
    failure_threshold: AtomicU32,
    /// The cooldown in nanoseconds (saturating: about 584 years).
    cooldown_nanos: AtomicU64,
}

impl LiveConfig {
    fn new(config: BreakerConfig) -> Self {
        Self {
            failure_threshold: AtomicU32::new(config.failure_threshold),
            cooldown_nanos: AtomicU64::new(nanos(config.cooldown)),
        }
    }

    fn store(&self, config: BreakerConfig) {
        self.failure_threshold
            .store(config.failure_threshold, Ordering::Relaxed);
        self.cooldown_nanos
            .store(nanos(config.cooldown), Ordering::Relaxed);
    }

    fn load(&self) -> BreakerConfig {
        BreakerConfig {
            failure_threshold: self.failure_threshold(),
            cooldown: self.cooldown(),
        }
    }

    fn failure_threshold(&self) -> u32 {
        self.failure_threshold.load(Ordering::Relaxed)
    }

    fn cooldown(&self) -> Duration {
        Duration::from_nanos(self.cooldown_nanos.load(Ordering::Relaxed))
    }
}

fn nanos(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
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
    /// Proceed: the breaker is Closed.
    Allowed,
    /// Proceed as the single Half-Open probe. The marker goes back to
    /// [`CircuitBreaker::on_neutral`] if the call ends without a verdict.
    Probe(Probe),
    /// Short-circuit; advertise roughly how long until a probe is admitted.
    Rejected { retry_after: Duration },
}

impl Admission {
    /// The probe marker, when this admission is the half-open probe.
    #[must_use]
    pub const fn probe(self) -> Option<Probe> {
        match self {
            Admission::Probe(probe) => Some(probe),
            Admission::Allowed | Admission::Rejected { .. } => None,
        }
    }
}

/// Identifies one admitted half-open probe, so only the call that holds it
/// can hand it back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Probe(u64);

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
    /// Number of probes admitted so far: the id of the outstanding one.
    probe_seq: u64,
}

impl Inner {
    /// Admit the caller as the half-open probe.
    fn start_probe(&mut self, now: Instant) -> Admission {
        self.probe_in_flight = true;
        self.probe_admitted_at = Some(now);
        self.probe_seq = self.probe_seq.wrapping_add(1);
        Admission::Probe(Probe(self.probe_seq))
    }
}

/// One circuit breaker, keyed externally by (provider, model).
#[derive(Debug)]
pub struct CircuitBreaker {
    inner: Mutex<Inner>,
    /// Shared with the registry, so a reload retunes this breaker in place.
    config: Arc<LiveConfig>,
    provider: String,
    model: String,
    metrics: Option<ResilienceMetrics>,
}

impl CircuitBreaker {
    fn new(
        provider: String,
        model: String,
        config: Arc<LiveConfig>,
        metrics: Option<ResilienceMetrics>,
    ) -> Self {
        Self {
            inner: Mutex::new(Inner {
                state: CircuitState::Closed,
                consecutive_failures: 0,
                opened_at: None,
                probe_in_flight: false,
                probe_admitted_at: None,
                probe_seq: 0,
            }),
            config,
            provider,
            model,
            metrics,
        }
    }

    /// Lock, recovering from a poisoned mutex rather than panicking (a panic in
    /// the request path is forbidden - CLAUDE.md rule 1). The guarded data is
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
                    .map_or(now, |opened| opened + self.config.cooldown());
                if now >= ready_at {
                    inner.state = CircuitState::HalfOpen;
                    let admission = inner.start_probe(now);
                    drop(inner);
                    self.report(CircuitState::HalfOpen);
                    admission
                } else {
                    Admission::Rejected {
                        retry_after: ready_at.saturating_duration_since(now),
                    }
                }
            }
            CircuitState::HalfOpen => {
                // A probe already in flight blocks others - unless it has been
                // outstanding longer than the cooldown, in which case it is
                // presumed lost (no on_success/on_failure ever ran) and a fresh
                // probe is admitted so the breaker cannot wedge shut.
                let cooldown = self.config.cooldown();
                let expires_at = inner.probe_admitted_at.map(|t| t + cooldown);
                let stale = expires_at.is_none_or(|deadline| now >= deadline);
                if inner.probe_in_flight && !stale {
                    Admission::Rejected {
                        retry_after: expires_at
                            .map_or(cooldown, |d| d.saturating_duration_since(now)),
                    }
                } else {
                    inner.start_probe(now)
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
                if inner.consecutive_failures >= self.config.failure_threshold() {
                    inner.state = CircuitState::Open;
                    inner.opened_at = Some(now);
                    drop(inner);
                    self.report(CircuitState::Open);
                }
            }
            CircuitState::Open => {}
        }
    }

    /// Record a call that says nothing about the provider's health (a
    /// cancellation, a client fault, or a link the gateway refused before
    /// calling upstream). Leaves the state and the failure streak alone, but
    /// releases the half-open probe this call held (`probe`, from its
    /// [`Admission`]), so the next caller can probe right away instead of
    /// waiting for the probe to go stale. A call that does not hold the
    /// outstanding probe (admitted while the breaker was still closed, or
    /// holding a probe since presumed lost and replaced) releases nothing:
    /// otherwise its neutral outcome would let a second probe through while
    /// the first is still in flight.
    pub fn on_neutral(&self, probe: Option<Probe>) {
        let Some(Probe(id)) = probe else {
            return;
        };
        let mut inner = self.lock();
        if inner.state == CircuitState::HalfOpen && inner.probe_in_flight && inner.probe_seq == id {
            inner.probe_in_flight = false;
            inner.probe_admitted_at = None;
        }
    }

    /// The current state (for snapshots / observability).
    pub fn state(&self) -> CircuitState {
        self.lock().state
    }

    /// Whether the breaker holds nothing a fresh one would not: closed, with
    /// no failure streak.
    fn is_idle(&self) -> bool {
        let inner = self.lock();
        inner.state == CircuitState::Closed && inner.consecutive_failures == 0
    }
}

/// The registry key: a provider, the budget group whose team key pool serves
/// it (`None` for the platform's own key) and a model.
#[derive(Debug, PartialEq, Eq)]
struct Key {
    provider: String,
    scope: Option<String>,
    model: String,
}

/// A [`Key`] by reference, so the per-request lookup borrows the link's
/// strings instead of allocating an owned key (the map is keyed by
/// `Borrow<dyn KeyRef>`).
trait KeyRef {
    fn parts(&self) -> (&str, Option<&str>, &str);
}

impl KeyRef for Key {
    fn parts(&self) -> (&str, Option<&str>, &str) {
        (&self.provider, self.scope.as_deref(), &self.model)
    }
}

impl KeyRef for (&str, Option<&str>, &str) {
    fn parts(&self) -> (&str, Option<&str>, &str) {
        *self
    }
}

impl<'a> Borrow<dyn KeyRef + 'a> for Key {
    fn borrow(&self) -> &(dyn KeyRef + 'a) {
        self
    }
}

// `Borrow` requires the owned and the borrowed form to hash and compare
// alike: both go through `parts`.
impl Hash for Key {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.parts().hash(state);
    }
}

impl Hash for dyn KeyRef + '_ {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.parts().hash(state);
    }
}

impl PartialEq for dyn KeyRef + '_ {
    fn eq(&self, other: &Self) -> bool {
        self.parts() == other.parts()
    }
}

impl Eq for dyn KeyRef + '_ {}

/// Registry size past which creating a breaker first prunes the idle ones.
/// Platform breakers are bounded by the configured (provider, model) pairs;
/// team-scoped ones (ADR 018) grow with the budget groups that ever sent a
/// request.
const PRUNE_THRESHOLD: usize = 1024;

/// The process-wide set of breakers, created lazily per (provider, model).
#[derive(Debug)]
pub struct CircuitBreakers {
    map: DashMap<Key, Arc<CircuitBreaker>>,
    /// The size at which the next insert prunes (see [`PRUNE_THRESHOLD`]).
    prune_at: AtomicUsize,
    config: Arc<LiveConfig>,
    metrics: Option<ResilienceMetrics>,
}

impl CircuitBreakers {
    /// Build an empty registry with the given tuning and optional gauge sink.
    #[must_use]
    pub fn new(config: BreakerConfig, metrics: Option<ResilienceMetrics>) -> Self {
        Self {
            map: DashMap::new(),
            prune_at: AtomicUsize::new(PRUNE_THRESHOLD),
            config: Arc::new(LiveConfig::new(config)),
            metrics,
        }
    }

    /// Replace the tuning of every breaker, existing and future (the
    /// hot-reload entry point). State is untouched: an open circuit stays
    /// open, and its remaining cooldown is measured with the new value. A
    /// closed breaker whose failure streak already reaches a lowered
    /// threshold opens on its next failure.
    pub fn set_config(&self, config: BreakerConfig) {
        self.config.store(config);
    }

    /// The tuning currently in force.
    #[must_use]
    pub fn config(&self) -> BreakerConfig {
        self.config.load()
    }

    /// The breaker for a (provider, model), created on first use.
    #[must_use]
    pub fn get(&self, provider: &str, model: &str) -> Arc<CircuitBreaker> {
        self.get_scoped(provider, None, model)
    }

    /// The breaker for a (provider, model) as served by `scope`: `None` is
    /// the platform's own key (the [`get`](Self::get) breaker); `Some(group)`
    /// is that budget group's team key pool (ADR 018), a separate breaker so
    /// a team's failures and the platform's never trip each other. A scoped
    /// breaker does not feed the `lumen_circuit_state` gauge (one series per
    /// group would be unbounded).
    ///
    /// The lookup of an existing breaker borrows its arguments: nothing is
    /// allocated per request.
    #[must_use]
    pub fn get_scoped(
        &self,
        provider: &str,
        scope: Option<&str>,
        model: &str,
    ) -> Arc<CircuitBreaker> {
        let borrowed: &dyn KeyRef = &(provider, scope, model);
        if let Some(existing) = self.map.get(borrowed) {
            return Arc::clone(existing.value());
        }
        // First use of this key (cold): keep the registry bounded before
        // growing it. Done before `entry`, which locks a shard.
        if self.map.len() >= self.prune_at.load(Ordering::Relaxed) {
            self.prune();
            // If most entries are live, wait for the registry to double
            // rather than scanning it on every insert.
            self.prune_at.store(
                self.map.len().saturating_mul(2).max(PRUNE_THRESHOLD),
                Ordering::Relaxed,
            );
        }
        let breaker = Arc::new(CircuitBreaker::new(
            provider.to_owned(),
            model.to_owned(),
            Arc::clone(&self.config),
            if scope.is_some() {
                None
            } else {
                self.metrics.clone()
            },
        ));
        let key = Key {
            provider: provider.to_owned(),
            scope: scope.map(str::to_owned),
            model: model.to_owned(),
        };
        // `entry` keeps creation atomic against a racing insert of the same key.
        Arc::clone(self.map.entry(key).or_insert(breaker).value())
    }

    /// Evict the breakers that hold no state: closed, with no failure
    /// streak, and not in use by a request. Such a breaker is identical to
    /// the one its next request would create, so nothing is lost; an open or
    /// half-open circuit, a failure streak and a breaker a request is using
    /// are always kept. Runs on reload and when the registry passes
    /// [`PRUNE_THRESHOLD`], never per request. Returns how many were evicted.
    pub fn prune(&self) -> usize {
        let before = self.map.len();
        // `retain` holds each shard's write lock, and a lookup clones the
        // `Arc` under the same shard's read lock: a count of 1 here means no
        // request holds, or can be acquiring, this breaker.
        self.map
            .retain(|_, breaker| Arc::strong_count(breaker) > 1 || !breaker.is_idle());
        before.saturating_sub(self.map.len())
    }

    /// How many breakers the registry holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Whether the registry holds no breaker.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// A snapshot of every known breaker's state (observability; not the hot
    /// path): `(provider, model, state)`, where a team-scoped breaker's
    /// provider reads `"{provider}#group:{group}"`.
    #[must_use]
    pub fn snapshot(&self) -> Vec<(String, String, CircuitState)> {
        self.map
            .iter()
            .map(|e| {
                let key = e.key();
                let provider = key.scope.as_ref().map_or_else(
                    || key.provider.clone(),
                    |group| format!("{}#group:{group}", key.provider),
                );
                (provider, key.model.clone(), e.value().state())
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
        CircuitBreaker::new(
            "openai".to_owned(),
            "gpt-4o".to_owned(),
            Arc::new(LiveConfig::new(config())),
            None,
        )
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
        assert!(matches!(b.admit(after), Admission::Probe(_)));
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
        assert!(matches!(b.admit(after), Admission::Probe(_)));
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
        assert!(matches!(b.admit(probe_at), Admission::Probe(_))); // probe admitted…
                                                                   // …and simply never resolved. A second probe within the cooldown is
                                                                   // still rejected (single-probe guarantee holds inside the window).
        assert!(matches!(
            b.admit(probe_at + Duration::from_secs(1)),
            Admission::Rejected { .. }
        ));
        // But once the lost probe's own cooldown elapses, a fresh probe passes.
        assert!(
            matches!(
                b.admit(probe_at + Duration::from_secs(31)),
                Admission::Probe(_)
            ),
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
        assert!(matches!(b.admit(after), Admission::Probe(_))); // probe
        b.on_failure(after); // probe fails
        assert_eq!(b.state(), CircuitState::Open);
        // The cooldown restarts from the probe failure.
        assert!(matches!(
            b.admit(after + Duration::from_secs(1)),
            Admission::Rejected { .. }
        ));
        assert!(matches!(
            b.admit(after + Duration::from_secs(31)),
            Admission::Probe(_)
        ));
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
            other => panic!("should be rejected mid-cooldown: {other:?}"),
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

    #[tokio::test]
    async fn a_new_config_reaches_existing_breakers_and_keeps_their_state() {
        let breakers = CircuitBreakers::new(config(), None);
        let existing = breakers.get("openai", "gpt-4o");
        let t0 = Instant::now();
        existing.on_failure(t0);
        existing.on_failure(t0);
        assert_eq!(existing.state(), CircuitState::Closed);

        // Threshold 5 -> 3, cooldown 30 s -> 10 s: the breaker created before
        // the change opens on its third failure and probes after 10 s.
        breakers.set_config(BreakerConfig {
            failure_threshold: 3,
            cooldown: Duration::from_secs(10),
        });
        assert_eq!(breakers.config().failure_threshold, 3);
        existing.on_failure(t0);
        assert_eq!(existing.state(), CircuitState::Open, "streak kept");
        assert!(matches!(
            existing.admit(t0 + Duration::from_secs(9)),
            Admission::Rejected { .. }
        ));
        assert!(matches!(
            existing.admit(t0 + Duration::from_secs(11)),
            Admission::Probe(_)
        ));

        // A breaker created afterwards uses the new tuning too.
        let fresh = breakers.get("openai", "gpt-4o-mini");
        for _ in 0..3 {
            fresh.on_failure(t0);
        }
        assert_eq!(fresh.state(), CircuitState::Open);
    }

    #[tokio::test]
    async fn a_neutral_result_releases_the_probe_without_a_verdict() {
        let b = breaker();
        let t0 = Instant::now();
        for _ in 0..5 {
            b.on_failure(t0);
        }
        let after = t0 + Duration::from_secs(31);
        let probe = b.admit(after).probe();
        assert!(probe.is_some());
        b.on_neutral(probe);
        assert_eq!(b.state(), CircuitState::HalfOpen, "no verdict");
        assert!(
            matches!(b.admit(after), Admission::Probe(_)),
            "probe released"
        );
        // While closed it touches nothing: the streak survives.
        let c = breaker();
        for _ in 0..4 {
            c.on_failure(t0);
        }
        c.on_neutral(None);
        c.on_failure(t0);
        assert_eq!(c.state(), CircuitState::Open);
    }

    #[tokio::test]
    async fn only_the_probe_holder_releases_the_probe() {
        let b = breaker();
        let t0 = Instant::now();
        // A slow request admitted while the breaker was still closed.
        let early = b.admit(t0).probe();
        assert_eq!(early, None);
        for _ in 0..5 {
            b.on_failure(t0);
        }
        let after = t0 + Duration::from_secs(31);
        let probe = b.admit(after).probe();
        assert!(probe.is_some());
        // The early request ends with no verdict (a client disconnect): it
        // holds no probe, so the probe stays in flight and no second one is
        // admitted.
        b.on_neutral(early);
        assert!(matches!(b.admit(after), Admission::Rejected { .. }));

        // A probe presumed lost and replaced cannot release its successor.
        let later = after + Duration::from_secs(31);
        let second = b.admit(later).probe();
        assert!(second.is_some() && second != probe);
        b.on_neutral(probe);
        assert!(matches!(b.admit(later), Admission::Rejected { .. }));
        // The holder can.
        b.on_neutral(second);
        assert!(matches!(b.admit(later), Admission::Probe(_)));
    }

    #[tokio::test]
    async fn prune_evicts_only_breakers_without_state() {
        let breakers = CircuitBreakers::new(config(), None);
        let now = Instant::now();
        // Idle: closed, no streak, nobody holding it.
        let _ = breakers
            .get_scoped("openai", Some("idle"), "gpt-4o")
            .state();
        // A failure streak below the threshold.
        breakers
            .get_scoped("openai", Some("streak"), "gpt-4o")
            .on_failure(now);
        // An open circuit.
        for _ in 0..5 {
            breakers
                .get_scoped("openai", Some("open"), "gpt-4o")
                .on_failure(now);
        }
        // Idle, but a request is holding it.
        let in_use = breakers.get_scoped("openai", Some("busy"), "gpt-4o");
        assert_eq!(breakers.len(), 4);

        assert_eq!(breakers.prune(), 1);
        let mut left: Vec<String> = breakers
            .snapshot()
            .into_iter()
            .map(|(provider, _, _)| provider)
            .collect();
        left.sort();
        assert_eq!(
            left,
            vec![
                "openai#group:busy",
                "openai#group:open",
                "openai#group:streak"
            ]
        );
        // What was kept is still the same breaker, state included.
        assert!(Arc::ptr_eq(
            &in_use,
            &breakers.get_scoped("openai", Some("busy"), "gpt-4o")
        ));
        assert_eq!(
            breakers
                .get_scoped("openai", Some("open"), "gpt-4o")
                .state(),
            CircuitState::Open
        );
        breakers
            .get_scoped("openai", Some("streak"), "gpt-4o")
            .on_success();
        drop(in_use);
        assert_eq!(breakers.prune(), 2, "once idle and released, both go");
        assert_eq!(breakers.len(), 1);
    }

    #[tokio::test]
    async fn the_registry_stays_bounded_as_groups_come_and_go() {
        let breakers = CircuitBreakers::new(config(), None);
        let now = Instant::now();
        for _ in 0..5 {
            breakers
                .get_scoped("openai", Some("down"), "gpt-4o")
                .on_failure(now);
        }
        // Far more one-off groups than the threshold, each leaving an idle
        // breaker behind.
        for i in 0..(PRUNE_THRESHOLD * 5) {
            let _ = breakers
                .get_scoped("openai", Some(&format!("g{i}")), "gpt-4o")
                .state();
        }
        assert!(breakers.len() <= PRUNE_THRESHOLD, "{}", breakers.len());
        assert_eq!(
            breakers
                .get_scoped("openai", Some("down"), "gpt-4o")
                .state(),
            CircuitState::Open,
            "live state survives every prune"
        );
    }

    #[tokio::test]
    async fn the_key_tells_provider_scope_and_model_apart() {
        let breakers = CircuitBreakers::new(config(), None);
        let a = breakers.get_scoped("openai", Some("g1"), "gpt-4o");
        // Same strings, shifted across the parts: all distinct breakers.
        for (provider, scope, model) in [
            ("openai", None, "gpt-4o"),
            ("openai", Some("g2"), "gpt-4o"),
            ("openai", Some("g1"), "gpt-4o-mini"),
            ("openai#group:g1", None, "gpt-4o"),
            ("g1", Some("openai"), "gpt-4o"),
        ] {
            assert!(!Arc::ptr_eq(
                &a,
                &breakers.get_scoped(provider, scope, model)
            ));
        }
        assert!(Arc::ptr_eq(
            &a,
            &breakers.get_scoped("openai", Some("g1"), "gpt-4o")
        ));
        assert_eq!(breakers.len(), 6);
    }

    #[tokio::test]
    async fn scoped_breakers_are_independent_of_the_platform_one() {
        let breakers = CircuitBreakers::new(config(), None);
        let now = Instant::now();
        for _ in 0..5 {
            breakers
                .get_scoped("openai", Some("g1"), "gpt-4o")
                .on_failure(now);
        }
        assert_eq!(
            breakers.get_scoped("openai", Some("g1"), "gpt-4o").state(),
            CircuitState::Open
        );
        assert_eq!(
            breakers.get("openai", "gpt-4o").state(),
            CircuitState::Closed
        );
        assert!(Arc::ptr_eq(
            &breakers.get_scoped("openai", None, "gpt-4o"),
            &breakers.get("openai", "gpt-4o")
        ));
    }
}
