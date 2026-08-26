//! Outbound budget-event signals (ADR 011).
//!
//! A billing control plane needs to hear about a budget *before* it reaches
//! zero: hard budgets refuse with `LM-4001` the moment the pool is empty, so
//! an auto-recharge has to fire earlier. This module is the detection and
//! queueing half of that: the entries in [`state`](crate::state) call into it
//! at budget settle time, and a sender task in the server crate drains the
//! queue and delivers the events over HTTP.
//!
//! Three rules shape everything here (ADR 011 §2):
//!
//! * **Never on the request path.** Detection is a compare against the armed
//!   thresholds on the atomic settle that already happens per request, and
//!   queueing is a non-blocking `try_send` into a **bounded** channel - the
//!   same discipline as the usage-log writer (CLAUDE.md rule 4). A full
//!   channel DROPS the event and counts it; it never applies backpressure.
//! * **Edge-triggered.** A crossed threshold fires exactly once per budget
//!   epoch, not once per request beyond it. Thresholds are sorted ascending,
//!   so the armed state is a prefix and one `fetch_max` both detects and
//!   claims a crossing - two concurrent settles can never double-fire.
//! * **Accounting facts only.** A payload carries the subject's id, name and
//!   budget numbers. Never a plaintext key, never request metadata, never
//!   prompt or response content.

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::state::micro_to_usd;

/// Which budget lifecycle event fired (ADR 011 §1).
///
/// The serialized form is the dotted name an operator writes in
/// `webhooks.events` and a receiver matches on (`"budget.threshold"`).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub enum EventKind {
    /// A key's or group's `budget_spent / budget_max` crossed a configured
    /// percentage. The trigger for a Stripe-style auto-recharge.
    #[serde(rename = "budget.threshold")]
    BudgetThreshold,
    /// The first admission refused with `LM-4001` since the subject last had
    /// headroom: the budget is gone and requests are now being refused.
    #[serde(rename = "budget.exhausted")]
    BudgetExhausted,
    /// A key was disabled through the admin API.
    #[serde(rename = "key.disabled")]
    KeyDisabled,
    /// A key's secret was rotated (its id and budget state are unchanged).
    #[serde(rename = "key.rotated")]
    KeyRotated,
    /// A key was soft-deleted and stops authenticating.
    #[serde(rename = "key.deleted")]
    KeyDeleted,
}

impl EventKind {
    /// Every event kind, for config validation and docs.
    pub const ALL: [Self; 5] = [
        Self::BudgetThreshold,
        Self::BudgetExhausted,
        Self::KeyDisabled,
        Self::KeyRotated,
        Self::KeyDeleted,
    ];

    /// The dotted wire name (`"budget.threshold"`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BudgetThreshold => "budget.threshold",
            Self::BudgetExhausted => "budget.exhausted",
            Self::KeyDisabled => "key.disabled",
            Self::KeyRotated => "key.rotated",
            Self::KeyDeleted => "key.deleted",
        }
    }
}

impl fmt::Display for EventKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Whether an event is about a single key or a shared budget group (ADR 009).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EventScope {
    /// One virtual key's own budget.
    Key,
    /// A budget group's shared pool.
    Group,
}

impl EventScope {
    /// The wire name (`"key"` / `"group"`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Key => "key",
            Self::Group => "group",
        }
    }
}

/// One delivered event. Accounting facts only - see the module docs.
///
/// `id` is minted once per event, not per delivery attempt, so a receiver can
/// deduplicate retries (and the re-fire a crash-and-restart can cause, since
/// enforcement state is flushed on an interval) by that id alone.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct BudgetEvent {
    /// Unique event id, stable across delivery attempts.
    pub id: String,
    /// What happened.
    pub event: EventKind,
    /// Whether `subject_id` names a key or a group.
    pub scope: EventScope,
    /// The key or group id.
    pub subject_id: String,
    /// The key or group's human-readable name.
    pub subject_name: String,
    /// Hard budget in USD; omitted when the subject has no cap.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget_max: Option<f64>,
    /// Spend accrued so far, in USD.
    pub budget_spent: f64,
    /// The threshold percentage that was crossed; only on
    /// [`EventKind::BudgetThreshold`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub threshold: Option<u8>,
    /// When the event was detected, unix seconds.
    pub ts: i64,
}

/// Where the live webhook settings came from (ADR 011 amendment §2). Reported
/// by `GET /admin/webhooks` so drift between the config file and the running
/// configuration is visible rather than inferred.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingsSource {
    /// A row written through `PUT /admin/webhooks`, which wins over the file.
    Database,
    /// The `[webhooks]` block of the config file.
    Config,
    /// Neither: webhooks are off.
    None,
}

/// Which surface a set of settings arrived on, for the one validation rule
/// that differs between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsOrigin {
    /// The `[webhooks]` block of a config file.
    ConfigFile,
    /// A `PUT /admin/webhooks` body.
    AdminApi,
}

/// The complete webhook configuration.
///
/// One type serves four roles - the `[webhooks]` TOML block, the
/// `PUT /admin/webhooks` request body, the `webhook_config` database row, and
/// the input the delivery pipeline is built from - so a field can never mean
/// one thing in a file and another over the wire.
///
/// It holds the *name* of the environment variable carrying the HMAC secret,
/// never the secret itself: a config file, an API response and a database row
/// are all things that get copied around.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebhookSettings {
    /// Receiver endpoint. One receiver; a backend needing fan-out does it
    /// itself (ADR 011 §5).
    pub url: String,
    /// Name of the environment variable holding the HMAC-SHA256 signing
    /// secret. `None` means the secret comes from
    /// `PUT /admin/webhooks/signing-key`, or that deliveries are unsigned.
    #[serde(default)]
    pub signing_key_env: Option<String>,
    /// Which event kinds to deliver.
    #[serde(default = "default_events")]
    pub events: Vec<EventKind>,
    /// Percent-of-budget thresholds for `budget.threshold`, each fired once
    /// per budget epoch.
    #[serde(default = "default_thresholds")]
    pub thresholds: Vec<u8>,
    /// Bounded queue capacity. A full queue DROPS events and counts them
    /// rather than slowing a request down.
    #[serde(default = "default_channel_capacity")]
    pub channel_capacity: usize,
    /// Per-attempt request timeout in ms.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    /// Total delivery attempts per event, including the first (`1` = no
    /// retries).
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,
    /// Base backoff delay in ms: the pre-jitter wait after the first failed
    /// attempt, doubling per attempt.
    #[serde(default = "default_retry_base_ms")]
    pub retry_base_ms: u64,
}

fn default_events() -> Vec<EventKind> {
    vec![EventKind::BudgetThreshold, EventKind::BudgetExhausted]
}
fn default_thresholds() -> Vec<u8> {
    vec![50, 80, 95]
}
const fn default_channel_capacity() -> usize {
    1_024
}
const fn default_timeout_ms() -> u64 {
    5_000
}
const fn default_max_attempts() -> u32 {
    5
}
const fn default_retry_base_ms() -> u64 {
    500
}

impl Default for WebhookSettings {
    fn default() -> Self {
        Self {
            url: String::new(),
            signing_key_env: None,
            events: default_events(),
            thresholds: default_thresholds(),
            channel_capacity: default_channel_capacity(),
            timeout_ms: default_timeout_ms(),
            max_attempts: default_max_attempts(),
            retry_base_ms: default_retry_base_ms(),
        }
    }
}

impl WebhookSettings {
    /// Validate the settings, returning a message naming the offending field.
    ///
    /// The caller wraps the message in its own error type: a config-file load
    /// turns it into a `Validation` error, an admin `PUT` into a 400
    /// `LM-1001`. One implementation, so the two surfaces can never drift
    /// into accepting different things.
    ///
    /// # Errors
    /// A human-readable message naming the field that is wrong.
    pub fn validate(&self, origin: SettingsOrigin) -> Result<(), String> {
        let url = self.url.trim();
        if url.is_empty() {
            return Err("webhooks.url must not be empty".to_owned());
        }
        if url != self.url {
            return Err("webhooks.url must not have leading or trailing whitespace".to_owned());
        }
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err("webhooks.url must be an http:// or https:// URL".to_owned());
        }
        self.validate_signing_key_env(origin)?;
        if self.events.is_empty() {
            return Err("webhooks.events must list at least one event kind".to_owned());
        }
        if self.events.contains(&EventKind::BudgetThreshold) {
            if self.thresholds.is_empty() {
                return Err(
                    "webhooks.thresholds must not be empty when 'budget.threshold' is enabled"
                        .to_owned(),
                );
            }
            if let Some(bad) = self.thresholds.iter().find(|t| **t == 0 || **t > 100) {
                return Err(format!(
                    "webhooks.thresholds must be percentages in 1..=100 (found {bad})"
                ));
            }
        }
        if self.channel_capacity == 0 {
            return Err("webhooks.channel_capacity must not be 0".to_owned());
        }
        for (field, value) in [
            ("webhooks.timeout_ms", self.timeout_ms),
            ("webhooks.max_attempts", u64::from(self.max_attempts)),
            ("webhooks.retry_base_ms", self.retry_base_ms),
        ] {
            if value == 0 {
                return Err(format!("{field} must not be 0"));
            }
        }
        Ok(())
    }

    /// The `signing_key_env` rules, including the one that differs by origin.
    fn validate_signing_key_env(&self, origin: SettingsOrigin) -> Result<(), String> {
        let Some(var) = &self.signing_key_env else {
            return Ok(());
        };
        if var.trim().is_empty() || var.trim() != var {
            return Err(
                "webhooks.signing_key_env must be a non-blank env var name with no \
                        surrounding whitespace"
                    .to_owned(),
            );
        }
        if !var.starts_with("LUMEN_") {
            return Ok(());
        }
        // `LUMEN_`-prefixed variables live in the config loader's own overlay
        // namespace, and are only kept out of it by being named in the file it
        // is reading. That works for a file-declared variable and cannot work
        // for a database-declared one: at boot the config is parsed before the
        // database is open, so nothing knows the stored name yet and the
        // variable would be misread as a config key. Steer an API-managed
        // secret to `PUT /admin/webhooks/signing-key` (no variable at all) or
        // to a name outside the prefix.
        if origin == SettingsOrigin::AdminApi {
            return Err(format!(
                "webhooks.signing_key_env '{var}' must not start with 'LUMEN_' when set through \
                 the admin API: that prefix is the config loader's namespace and cannot be \
                 excluded from it before the database is read. Use PUT \
                 /admin/webhooks/signing-key to store the secret directly, or name the variable \
                 without the prefix"
            ));
        }
        if var.contains("__") {
            return Err(format!(
                "webhooks.signing_key_env '{var}' must not contain '__': LUMEN_-prefixed \
                 variables are parsed as nested config keys on '__'"
            ));
        }
        Ok(())
    }
}

/// Mint a fresh event id: `evt_` + 16 random bytes in hex.
fn new_event_id() -> String {
    use rand::Rng as _;
    let mut bytes = [0_u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    format!("evt_{:032x}", u128::from_be_bytes(bytes))
}

/// Prometheus hook for the queue, so `lumen_auth` never has to depend on the
/// telemetry crate to count what it drops. The server implements this over
/// its `WebhookMetrics`; tests use [`NoopCounters`].
pub trait SignalCounters: Send + Sync + fmt::Debug {
    /// One event accepted into the bounded queue.
    fn inc_queued(&self);
    /// One event dropped because the queue was full (or the sender is gone).
    fn inc_dropped(&self);
}

/// A [`SignalCounters`] that counts nothing - for tests and for builds with
/// no metrics registry to hand.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopCounters;

impl SignalCounters for NoopCounters {
    fn inc_queued(&self) {}
    fn inc_dropped(&self) {}
}

/// The bounded queue events are handed to, created once at boot and never
/// swapped: a hot reload replaces the [`BudgetSignals`] *policy* around it,
/// so the sender task keeps the receiver it was spawned with (ADR 011 §4).
#[derive(Debug)]
pub struct SignalQueue {
    tx: mpsc::Sender<BudgetEvent>,
    counters: Arc<dyn SignalCounters>,
}

impl SignalQueue {
    /// Create the queue and hand back the receiver for the sender task.
    /// `capacity` is clamped to at least 1 (a zero-capacity channel would
    /// drop every event).
    #[must_use]
    pub fn new(
        capacity: usize,
        counters: Arc<dyn SignalCounters>,
    ) -> (Arc<Self>, mpsc::Receiver<BudgetEvent>) {
        let (tx, rx) = mpsc::channel(capacity.max(1));
        (Arc::new(Self { tx, counters }), rx)
    }

    /// Enqueue one event without ever waiting. A full queue drops it and
    /// counts the drop: webhooks are a convenience signal, and
    /// `GET /admin/usage/export` remains the system of record (ADR 011 §2).
    fn emit(&self, event: BudgetEvent) {
        // `try_send` is the whole point: the request path must never block on
        // a slow or absent webhook receiver.
        match self.tx.try_send(event) {
            Ok(()) => self.counters.inc_queued(),
            Err(error) => {
                self.counters.inc_dropped();
                // The event itself is safe to name (ids and a kind, no
                // secrets); the payload numbers are not logged.
                tracing::debug!(
                    reason = %match error {
                        mpsc::error::TrySendError::Full(_) => "queue full",
                        mpsc::error::TrySendError::Closed(_) => "sender stopped",
                    },
                    "budget event dropped"
                );
            }
        }
    }
}

/// The live signalling policy: the queue plus which events and thresholds are
/// enabled. Held behind a [`SignalCell`] so a hot reload swaps it atomically
/// and `None` (no `[webhooks]` block) means zero detection work.
#[derive(Debug)]
pub struct BudgetSignals {
    queue: Arc<SignalQueue>,
    /// Enabled event kinds. A handful of entries, so a linear scan beats a set.
    events: Vec<EventKind>,
    /// Percent thresholds, ascending and deduplicated. Ascending order is
    /// load-bearing: the armed state is the length of the crossed prefix.
    thresholds: Vec<u8>,
}

impl BudgetSignals {
    /// Build a policy over `queue`. `thresholds` is sorted and deduplicated
    /// here, so callers may pass an operator's list verbatim.
    #[must_use]
    pub fn new(queue: Arc<SignalQueue>, events: &[EventKind], thresholds: &[u8]) -> Self {
        let mut thresholds = thresholds.to_vec();
        thresholds.sort_unstable();
        thresholds.dedup();
        Self {
            queue,
            events: events.to_vec(),
            thresholds,
        }
    }

    /// Whether `kind` is enabled for delivery.
    #[must_use]
    pub fn wants(&self, kind: EventKind) -> bool {
        self.events.contains(&kind)
    }

    /// The configured thresholds, ascending.
    #[must_use]
    pub fn thresholds(&self) -> &[u8] {
        &self.thresholds
    }

    /// Emit `kind` for `subject`, if that kind is enabled. Used for the
    /// administrative lifecycle events, which have no threshold and no
    /// edge-trigger state of their own (each admin action IS the edge).
    pub fn emit(&self, kind: EventKind, subject: &Subject<'_>) {
        if !self.wants(kind) {
            return;
        }
        self.queue.emit(subject.event(kind, None));
    }
}

/// A cell holding the live [`BudgetSignals`]; `None` = webhooks off, which is
/// the default and costs one pointer load per settle.
pub type SignalCell = arc_swap::ArcSwapOption<BudgetSignals>;

/// The identity and budget numbers an event is built from. Borrowed, so
/// building one on the settle path allocates nothing when no event fires.
#[derive(Debug, Clone, Copy)]
pub struct Subject<'a> {
    /// Key or group.
    pub scope: EventScope,
    /// The subject's opaque id.
    pub id: &'a str,
    /// The subject's human-readable name.
    pub name: &'a str,
    /// Hard budget in micro-USD; `None` = uncapped.
    pub max_micro: Option<i64>,
    /// Spend accrued so far, in micro-USD.
    pub spent_micro: i64,
}

impl Subject<'_> {
    /// Materialise an event for this subject.
    fn event(&self, kind: EventKind, threshold: Option<u8>) -> BudgetEvent {
        BudgetEvent {
            id: new_event_id(),
            event: kind,
            scope: self.scope,
            subject_id: self.id.to_owned(),
            subject_name: self.name.to_owned(),
            budget_max: self.max_micro.map(micro_to_usd),
            budget_spent: micro_to_usd(self.spent_micro),
            threshold,
            ts: crate::now_unix(),
        }
    }

    /// Percentage of the cap consumed, or `None` when uncapped. Computed in
    /// `i128` so a huge budget cannot overflow the `* 100`.
    fn consumed_percent(&self) -> Option<i64> {
        let max = self.max_micro?;
        if max <= 0 {
            // A zero (or nonsensical negative) cap is fully consumed by
            // definition: any spend at all is over it.
            return Some(100);
        }
        let percent = i128::from(self.spent_micro.max(0)) * 100 / i128::from(max);
        Some(i64::try_from(percent).unwrap_or(i64::MAX))
    }
}

/// Per-subject edge-trigger state, embedded in each live key and group entry.
///
/// `crossed` is the number of thresholds already fired in the current budget
/// epoch. Because the threshold list is ascending, that count is exactly the
/// armed prefix, which makes a single `fetch_max` both the detection and the
/// claim: whichever concurrent settle raises the count wins, and the loser
/// emits nothing.
#[derive(Debug, Default)]
pub struct SignalState {
    crossed: AtomicU32,
    exhausted: AtomicBool,
}

impl SignalState {
    /// Fire a `budget.threshold` event for every threshold this settle newly
    /// crossed. Nothing to do for an uncapped subject, and nothing allocated
    /// when no threshold moved.
    pub fn on_settle(&self, signals: &BudgetSignals, subject: &Subject<'_>) {
        if !signals.wants(EventKind::BudgetThreshold) {
            return;
        }
        let Some(percent) = subject.consumed_percent() else {
            return;
        };
        let reached = crossed_count(signals.thresholds(), percent);
        // `fetch_max` claims the crossing: only the caller that actually
        // raised the count emits, so a threshold fires once per epoch even
        // under concurrent settles.
        let previous = self.crossed.fetch_max(reached, Ordering::SeqCst);
        if previous >= reached {
            return;
        }
        let (from, to) = (previous as usize, reached as usize);
        for threshold in &signals.thresholds()[from..to] {
            signals
                .queue
                .emit(subject.event(EventKind::BudgetThreshold, Some(*threshold)));
        }
    }

    /// Fire `budget.exhausted` for the first `LM-4001` refusal since this
    /// subject last had headroom. Later refusals are silent until a grant or
    /// a limit change re-arms the signal.
    pub fn on_refusal(&self, signals: &BudgetSignals, subject: &Subject<'_>) {
        if !signals.wants(EventKind::BudgetExhausted) {
            return;
        }
        if self.exhausted.swap(true, Ordering::SeqCst) {
            return;
        }
        signals
            .queue
            .emit(subject.event(EventKind::BudgetExhausted, None));
    }

    /// Re-arm both signals against a changed cap: a new budget epoch (ADR 011
    /// §1). A grant that buys headroom lowers the consumed percentage, which
    /// disarms every threshold it drops below and re-arms `budget.exhausted`;
    /// a cap *reduction* leaves the already-fired thresholds fired.
    ///
    /// Called from the admin grant/patch paths only, never from the request
    /// path: an unsettled reservation's refund deliberately does NOT re-arm,
    /// so a failed call cannot make a threshold flap.
    pub fn rearm(&self, thresholds: &[u8], subject: &Subject<'_>) {
        let reached = subject
            .consumed_percent()
            .map_or(0, |percent| crossed_count(thresholds, percent));
        self.crossed.store(reached, Ordering::SeqCst);
        let has_headroom = subject
            .max_micro
            .is_none_or(|max| subject.spent_micro < max);
        if has_headroom {
            self.exhausted.store(false, Ordering::SeqCst);
        }
    }
}

/// How many of the (ascending) `thresholds` `percent` has reached.
fn crossed_count(thresholds: &[u8], percent: i64) -> u32 {
    let count = thresholds
        .iter()
        .take_while(|t| i64::from(**t) <= percent)
        .count();
    u32::try_from(count).unwrap_or(u32::MAX)
}

#[cfg(test)]
// Exact micro-USD values converted to f64 and compared back - the conversion
// is exact for these magnitudes, so strict equality is the right assertion.
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    fn queue() -> (Arc<SignalQueue>, mpsc::Receiver<BudgetEvent>) {
        SignalQueue::new(16, Arc::new(NoopCounters))
    }

    fn signals(queue: &Arc<SignalQueue>) -> BudgetSignals {
        BudgetSignals::new(Arc::clone(queue), &EventKind::ALL, &[50, 80, 95])
    }

    fn subject(spent: i64, max: Option<i64>) -> Subject<'static> {
        Subject {
            scope: EventScope::Key,
            id: "key-1",
            name: "prepaid",
            max_micro: max,
            spent_micro: spent,
        }
    }

    fn drain(rx: &mut mpsc::Receiver<BudgetEvent>) -> Vec<BudgetEvent> {
        let mut out = Vec::new();
        while let Ok(event) = rx.try_recv() {
            out.push(event);
        }
        out
    }

    #[test]
    fn thresholds_are_sorted_and_deduplicated_on_construction() {
        let (q, _rx) = queue();
        let s = BudgetSignals::new(q, &[EventKind::BudgetThreshold], &[95, 50, 50, 80]);
        assert_eq!(s.thresholds(), [50, 80, 95]);
    }

    #[test]
    fn crossing_a_threshold_fires_once_however_many_settles_follow() {
        let (q, mut rx) = queue();
        let s = signals(&q);
        let state = SignalState::default();

        // 60 % of a $100 cap: only the 50 % threshold.
        state.on_settle(&s, &subject(60_000_000, Some(100_000_000)));
        let fired = drain(&mut rx);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].threshold, Some(50));
        assert_eq!(fired[0].event, EventKind::BudgetThreshold);
        assert_eq!(fired[0].budget_max, Some(100.0));
        assert_eq!(fired[0].budget_spent, 60.0);

        // More spend, still under 80 %: nothing new.
        state.on_settle(&s, &subject(70_000_000, Some(100_000_000)));
        assert!(drain(&mut rx).is_empty());

        // Past 80 %: exactly the one new crossing.
        state.on_settle(&s, &subject(81_000_000, Some(100_000_000)));
        let fired = drain(&mut rx);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].threshold, Some(80));
    }

    #[test]
    fn a_settle_that_jumps_several_thresholds_fires_each_one_once() {
        let (q, mut rx) = queue();
        let s = signals(&q);
        let state = SignalState::default();

        state.on_settle(&s, &subject(99_000_000, Some(100_000_000)));
        let fired: Vec<Option<u8>> = drain(&mut rx).into_iter().map(|e| e.threshold).collect();
        assert_eq!(fired, [Some(50), Some(80), Some(95)]);

        state.on_settle(&s, &subject(100_000_000, Some(100_000_000)));
        assert!(drain(&mut rx).is_empty());
    }

    #[test]
    fn a_grant_that_buys_headroom_rearms_the_thresholds_it_drops_below() {
        let (q, mut rx) = queue();
        let s = signals(&q);
        let state = SignalState::default();

        state.on_settle(&s, &subject(96_000_000, Some(100_000_000)));
        assert_eq!(drain(&mut rx).len(), 3);

        // Cap doubled: 96/200 = 48 %, below every threshold.
        state.rearm(s.thresholds(), &subject(96_000_000, Some(200_000_000)));
        // Crossing 50 % of the NEW cap fires again - a new epoch.
        state.on_settle(&s, &subject(101_000_000, Some(200_000_000)));
        let fired: Vec<Option<u8>> = drain(&mut rx).into_iter().map(|e| e.threshold).collect();
        assert_eq!(fired, [Some(50)]);
    }

    #[test]
    fn an_uncapped_subject_never_fires_a_threshold() {
        let (q, mut rx) = queue();
        let s = signals(&q);
        let state = SignalState::default();
        state.on_settle(&s, &subject(999_000_000, None));
        assert!(drain(&mut rx).is_empty());
    }

    #[test]
    fn exhaustion_fires_once_until_a_grant_rearms_it() {
        let (q, mut rx) = queue();
        let s = signals(&q);
        let state = SignalState::default();

        state.on_refusal(&s, &subject(100_000_000, Some(100_000_000)));
        state.on_refusal(&s, &subject(100_000_000, Some(100_000_000)));
        let fired = drain(&mut rx);
        assert_eq!(fired.len(), 1, "exhaustion must be edge-triggered");
        assert_eq!(fired[0].event, EventKind::BudgetExhausted);
        assert_eq!(fired[0].threshold, None);

        // A grant restores headroom, so the NEXT exhaustion is a new event.
        state.rearm(s.thresholds(), &subject(100_000_000, Some(150_000_000)));
        state.on_refusal(&s, &subject(150_000_000, Some(150_000_000)));
        assert_eq!(drain(&mut rx).len(), 1);
    }

    #[test]
    fn a_rearm_without_headroom_keeps_exhaustion_silent() {
        let (q, mut rx) = queue();
        let s = signals(&q);
        let state = SignalState::default();
        state.on_refusal(&s, &subject(100_000_000, Some(100_000_000)));
        assert_eq!(drain(&mut rx).len(), 1);
        // A PATCH that lowers the cap further buys no headroom.
        state.rearm(s.thresholds(), &subject(100_000_000, Some(50_000_000)));
        state.on_refusal(&s, &subject(100_000_000, Some(50_000_000)));
        assert!(drain(&mut rx).is_empty());
    }

    #[test]
    fn disabled_event_kinds_emit_nothing() {
        let (q, mut rx) = queue();
        // Only lifecycle events enabled.
        let s = BudgetSignals::new(Arc::clone(&q), &[EventKind::KeyDeleted], &[50]);
        let state = SignalState::default();
        state.on_settle(&s, &subject(90_000_000, Some(100_000_000)));
        state.on_refusal(&s, &subject(100_000_000, Some(100_000_000)));
        assert!(drain(&mut rx).is_empty());

        s.emit(
            EventKind::KeyDeleted,
            &subject(1_000_000, Some(100_000_000)),
        );
        let fired = drain(&mut rx);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].event, EventKind::KeyDeleted);
    }

    #[test]
    fn a_full_queue_drops_events_and_counts_them() {
        #[derive(Debug, Default)]
        struct Counting {
            queued: AtomicU32,
            dropped: AtomicU32,
        }
        impl SignalCounters for Counting {
            fn inc_queued(&self) {
                self.queued.fetch_add(1, Ordering::SeqCst);
            }
            fn inc_dropped(&self) {
                self.dropped.fetch_add(1, Ordering::SeqCst);
            }
        }

        let counters = Arc::new(Counting::default());
        // Capacity 1: the second event has nowhere to go.
        let (q, _rx) = SignalQueue::new(1, counters.clone());
        let s = BudgetSignals::new(q, &EventKind::ALL, &[10, 20]);
        let state = SignalState::default();

        state.on_settle(&s, &subject(25_000_000, Some(100_000_000)));
        assert_eq!(counters.queued.load(Ordering::SeqCst), 1);
        assert_eq!(counters.dropped.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn event_ids_are_unique_per_event() {
        let (q, mut rx) = queue();
        let s = signals(&q);
        let state = SignalState::default();
        state.on_settle(&s, &subject(99_000_000, Some(100_000_000)));
        let ids: Vec<String> = drain(&mut rx).into_iter().map(|e| e.id).collect();
        assert_eq!(ids.len(), 3);
        assert!(ids
            .iter()
            .all(|id| id.starts_with("evt_") && id.len() == 36));
        let mut deduped = ids.clone();
        deduped.sort_unstable();
        deduped.dedup();
        assert_eq!(deduped.len(), 3, "event ids must be unique");
    }

    #[test]
    fn a_zero_cap_counts_as_fully_consumed() {
        let (q, mut rx) = queue();
        let s = signals(&q);
        let state = SignalState::default();
        state.on_settle(&s, &subject(0, Some(0)));
        let fired: Vec<Option<u8>> = drain(&mut rx).into_iter().map(|e| e.threshold).collect();
        assert_eq!(fired, [Some(50), Some(80), Some(95)]);
    }

    #[test]
    fn the_payload_serializes_to_the_documented_shape() {
        let event = BudgetEvent {
            id: "evt_1".to_owned(),
            event: EventKind::BudgetThreshold,
            scope: EventScope::Group,
            subject_id: "grp_1".to_owned(),
            subject_name: "acme".to_owned(),
            budget_max: Some(100.0),
            budget_spent: 80.5,
            threshold: Some(80),
            ts: 1_700_000_000,
        };
        let json = serde_json::to_value(&event).expect("serializes");
        assert_eq!(json["event"], "budget.threshold");
        assert_eq!(json["scope"], "group");
        assert_eq!(json["subject_id"], "grp_1");
        assert_eq!(json["threshold"], 80);
        assert_eq!(json["budget_spent"], 80.5);

        // An uncapped subject omits `budget_max` rather than sending null.
        let uncapped = BudgetEvent {
            budget_max: None,
            threshold: None,
            ..event
        };
        let json = serde_json::to_value(&uncapped).expect("serializes");
        assert!(json.get("budget_max").is_none());
        assert!(json.get("threshold").is_none());
    }

    #[test]
    fn event_kinds_round_trip_through_their_dotted_names() {
        for kind in EventKind::ALL {
            let json = serde_json::to_string(&kind).expect("serializes");
            assert_eq!(json, format!("\"{}\"", kind.as_str()));
            let back: EventKind = serde_json::from_str(&json).expect("deserializes");
            assert_eq!(back, kind);
        }
    }
}
