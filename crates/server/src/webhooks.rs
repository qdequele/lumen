//! Outbound webhook delivery for budget events (ADR 011).
//!
//! The detection half lives in [`lumen_auth::events`]: an entry compares its
//! spend against the armed thresholds on the atomic settle that already
//! happens per request, and pushes any crossing into a bounded queue. This
//! module owns the other half - one task that drains that queue and POSTs each
//! event to the configured receiver.
//!
//! Everything here is deliberately off the request path. The task is spawned
//! once at boot and lives as long as the process; it holds the only receiver,
//! so a slow or dead endpoint can never propagate backpressure into a request
//! (a full queue drops, see [`SignalQueue`](lumen_auth::events::SignalQueue)).
//!
//! Delivery semantics (ADR 011 §2, §3):
//!
//! * **At-least-once while the process lives.** Retryable failures back off
//!   exponentially with jitter up to `max_attempts`, then the event is dropped
//!   and `lumen_webhook_dead_total` increments. Nothing is persisted: the
//!   `GET /admin/usage/export` route remains the system of record.
//! * **Signed and deduplicable.** Each POST carries `x-lumen-signature` (hex
//!   HMAC-SHA256 over the exact request body), `x-lumen-event-id` (minted once
//!   per event, so retries and a post-restart re-fire are deduplicable) and
//!   `x-lumen-timestamp` (the event's own `ts`, which is inside the signed
//!   body, so the header cannot disagree with what was signed).
//! * **Cancellable.** Shutdown cancels the task through the same
//!   [`CancellationToken`] discipline as provider calls: the in-flight attempt
//!   is abandoned, the queue is not drained, and shutdown never blocks on a
//!   sick receiver.
//!
//! The signing secret is wrapped in [`SigningKey`], whose `Debug` is redacted
//! and whose bytes are zeroized on drop, so it can never reach a log line or
//! an error message. It comes from the environment variable named in the
//! settings, or - for a control plane that does not own the gateway's
//! environment - from `PUT /admin/webhooks/signing-key`, sealed at rest under
//! the master key (ADR 011 amendment §4).
//!
//! # The control surface
//!
//! [`WebhookController`] owns everything above and is the only thing `main`
//! keeps. It exists whenever auth does, but stays **inert** until webhooks are
//! first enabled: no queue, no sender task, and no Prometheus collector, so a
//! gateway that never enables webhooks exports no `lumen_webhook_*` series at
//! all. Every field is then editable at runtime
//! ([`apply`](WebhookController::apply)):
//!
//! * `url`, `events`, `thresholds` and the retry knobs swap in live cells;
//! * `channel_capacity` **rebuilds** the queue. The new queue takes new
//!   events; the previous sender keeps its receiver, drains what it already
//!   had under the settings it had, and exits when its last sender drops.
//!   Nothing already accepted is discarded to change a capacity;
//! * the signing secret lives in a cell the sender reads per attempt, so a
//!   rotation applies to the next delivery without restarting anything.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::{ArcSwap, ArcSwapOption};
use hmac::{Hmac, KeyInit, Mac};
use lumen_auth::crypto::MasterKey;
use lumen_auth::events::{
    BudgetEvent, BudgetSignals, SettingsOrigin, SettingsSource, SignalCounters, SignalQueue,
    WebhookSettings,
};
use lumen_auth::state::AuthState;
use lumen_auth::store::{KeyStore, StoredWebhookConfig};
use lumen_telemetry::{Metrics, WebhookMetrics};
use sha2::Sha256;
use tokio::sync::mpsc::Receiver;
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

/// Header carrying the hex HMAC-SHA256 of the request body.
const SIGNATURE_HEADER: &str = "x-lumen-signature";
/// Header carrying the per-event id (stable across delivery attempts).
const EVENT_ID_HEADER: &str = "x-lumen-event-id";
/// Header carrying the event kind, so a receiver can route without parsing.
const EVENT_HEADER: &str = "x-lumen-event";
/// Header carrying the event's unix-seconds timestamp (also inside the body).
const TIMESTAMP_HEADER: &str = "x-lumen-timestamp";

/// The HMAC signing secret.
///
/// STRICT rule 5: the secret must be unrepresentable through `Debug` and
/// `Display`, so a stray `{:?}` in a log line or an error chain cannot leak
/// it. The bytes are zeroized when the key is dropped.
pub struct SigningKey(Zeroizing<Vec<u8>>);

impl SigningKey {
    /// Wrap raw secret bytes.
    #[must_use]
    pub fn new(secret: impl Into<Vec<u8>>) -> Self {
        Self(Zeroizing::new(secret.into()))
    }

    /// Hex HMAC-SHA256 of `body` under this key.
    #[must_use]
    fn sign(&self, body: &[u8]) -> String {
        // `new_from_slice` only rejects invalid key *lengths*, and HMAC
        // accepts any length, so this cannot fail for SHA-256.
        let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(&self.0)
            .unwrap_or_else(|_| unreachable!("HMAC accepts a key of any length"));
        mac.update(body);
        hex::encode(mac.finalize().into_bytes())
    }
}

impl fmt::Debug for SigningKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SigningKey(REDACTED)")
    }
}

/// The parts of the webhook config a hot reload may change while the queue
/// and the sender task stay put.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryPolicy {
    /// Receiver endpoint.
    pub url: String,
    /// Per-attempt request timeout.
    pub timeout: Duration,
    /// Total attempts per event, including the first.
    pub max_attempts: u32,
    /// Pre-jitter wait after the first failed attempt; doubles per attempt.
    pub retry_base: Duration,
}

impl From<&WebhookSettings> for DeliveryPolicy {
    fn from(settings: &WebhookSettings) -> Self {
        Self {
            url: settings.url.clone(),
            timeout: Duration::from_millis(settings.timeout_ms),
            // Validation rejects 0, but a floor here keeps the loop sane for a
            // hand-edited database row too.
            max_attempts: settings.max_attempts.max(1),
            retry_base: Duration::from_millis(settings.retry_base_ms),
        }
    }
}

/// One live delivery pipeline: the bounded queue plus the cells its sender
/// task reads. Replaced wholesale by an apply; the sender task holds only the
/// cells, so a swap that keeps the same capacity never disturbs it.
#[derive(Debug)]
struct Pipeline {
    /// The settings this pipeline was built from, for `GET /admin/webhooks`
    /// and to decide whether the next apply can reuse the queue.
    settings: WebhookSettings,
    queue: Arc<SignalQueue>,
    policy: Arc<ArcSwap<DeliveryPolicy>>,
    signing: Arc<ArcSwapOption<SigningKey>>,
}

/// The webhook control surface: builds, retunes and tears down the delivery
/// pipeline, and answers `GET /admin/webhooks`.
///
/// Present whenever auth is (webhooks describe keys and groups, so they need
/// it), but inert until something enables them - see the module docs.
pub struct WebhookController {
    /// The Prometheus registry, so collectors can be registered on the first
    /// enable rather than at boot.
    registry: Metrics,
    /// The shared, pooled HTTP client.
    client: reqwest::Client,
    /// Process-lifetime shutdown token, handed to every sender task.
    cancel: CancellationToken,
    /// The live pipeline; `None` = webhooks off.
    live: ArcSwapOption<Pipeline>,
    /// Registered exactly once. Prometheus refuses a duplicate registration,
    /// and re-enabling webhooks must not be the thing that fails because of
    /// it, so the handle is kept and reused.
    metrics: std::sync::OnceLock<WebhookMetrics>,
    /// The database's view, refreshed by [`refresh_from_store`] at boot, on
    /// every config reload, and after each admin write. Cached because
    /// resolution happens on the synchronous reload path, which has no
    /// business awaiting a query.
    stored: ArcSwapOption<StoredWebhookConfig>,
    /// The sealed-at-rest signing secret, cached for the same reason.
    stored_secret: ArcSwapOption<SigningKey>,
    /// Serialises apply/disable. Two concurrent `PUT`s could otherwise
    /// interleave a queue rebuild with a metrics registration and leave the
    /// live cell pointing at a pipeline whose task was never spawned.
    apply_lock: std::sync::Mutex<()>,
}

impl fmt::Debug for WebhookController {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The signing cells are redacted by `SigningKey`'s own Debug, but keep
        // this minimal regardless.
        f.debug_struct("WebhookController")
            .field("enabled", &self.live.load().is_some())
            .finish_non_exhaustive()
    }
}

impl WebhookController {
    /// Create an inert controller. Nothing is registered or spawned until
    /// [`apply`](Self::apply).
    #[must_use]
    pub fn new(registry: Metrics, client: reqwest::Client, cancel: CancellationToken) -> Self {
        Self {
            registry,
            client,
            cancel,
            live: ArcSwapOption::empty(),
            metrics: std::sync::OnceLock::new(),
            stored: ArcSwapOption::empty(),
            stored_secret: ArcSwapOption::empty(),
            apply_lock: std::sync::Mutex::new(()),
        }
    }

    /// Refresh the cached database view: the stored settings row and the
    /// sealed signing secret.
    ///
    /// Runs at boot, in the reload task, and after an admin write - never on a
    /// request path. A failure leaves the previous cache in place and is
    /// logged, so a sick database cannot strip a working configuration.
    pub async fn refresh_from_store(&self, store: &KeyStore, master: Option<&MasterKey>) {
        match store.load_webhook_config().await {
            Ok(stored) => self.stored.store(stored.map(Arc::new)),
            Err(error) => {
                tracing::warn!(%error, "could not read the stored webhook config; keeping the previous one");
            }
        }
        let Some(master) = master else {
            return;
        };
        match store.load_webhook_secret(master).await {
            // Only the presence of a secret is ever logged, never its value.
            Ok(secret) => self
                .stored_secret
                .store(secret.map(|value| Arc::new(SigningKey::new(value.into_bytes())))),
            Err(error) => {
                tracing::warn!(%error, "could not read the stored webhook signing secret; keeping the previous one");
            }
        }
    }

    /// The cached stored row, if any.
    #[must_use]
    pub fn stored(&self) -> Option<Arc<StoredWebhookConfig>> {
        self.stored.load_full()
    }

    /// Whether a signing secret is sealed in the database. Reported by
    /// `GET /admin/webhooks`; the value itself is never exposed.
    #[must_use]
    pub fn has_stored_secret(&self) -> bool {
        self.stored_secret.load().is_some()
    }

    /// The settings currently in force, or `None` when webhooks are off.
    #[must_use]
    pub fn live_settings(&self) -> Option<WebhookSettings> {
        self.live.load().as_ref().map(|p| p.settings.clone())
    }

    /// Whether deliveries currently carry a signature.
    #[must_use]
    pub fn is_signed(&self) -> bool {
        self.live
            .load()
            .as_ref()
            .is_some_and(|p| p.signing.load().is_some())
    }

    /// Resolve which settings win and apply them (ADR 011 amendment §2): a
    /// stored row beats the `[webhooks]` file block, a stored row marked
    /// disabled means off whatever the file says, and no row falls back to the
    /// file.
    ///
    /// Returns the source that won, so the caller can log or report it.
    ///
    /// # Errors
    /// The winning settings do not validate, the signing secret cannot be
    /// resolved, or the Prometheus collectors cannot be registered. The
    /// previous pipeline is left running in every case.
    pub fn resolve_and_apply(
        &self,
        file_block: Option<&WebhookSettings>,
        keys: &AuthState,
    ) -> Result<SettingsSource, String> {
        match self.stored() {
            Some(stored) if stored.enabled => {
                self.apply(&stored.settings, keys)?;
                Ok(SettingsSource::Database)
            }
            // A stored row marked disabled shadows the file block on purpose:
            // a DELETE must not be undone by the next reload.
            Some(_) => {
                self.disable(keys);
                Ok(SettingsSource::None)
            }
            None => {
                if let Some(settings) = file_block {
                    self.apply(settings, keys)?;
                    Ok(SettingsSource::Config)
                } else {
                    self.disable(keys);
                    Ok(SettingsSource::None)
                }
            }
        }
    }

    /// Where the settings currently in force came from.
    #[must_use]
    pub fn source(&self) -> SettingsSource {
        if self.live.load().is_none() {
            return SettingsSource::None;
        }
        match self.stored() {
            Some(stored) if stored.enabled => SettingsSource::Database,
            _ => SettingsSource::Config,
        }
    }

    /// Build or retune the pipeline from `settings` and install the matching
    /// signalling policy on `keys`.
    ///
    /// Reuses the existing queue - and so the existing sender task - unless
    /// `channel_capacity` changed. When it did, a fresh queue and task are
    /// created and the old task drains its backlog before exiting; nothing
    /// already accepted is thrown away to resize a channel.
    ///
    /// # Errors
    /// Invalid settings, an unresolvable signing secret, or a metrics
    /// registration failure. Nothing is swapped on error.
    pub fn apply(&self, settings: &WebhookSettings, keys: &AuthState) -> Result<(), String> {
        // Defence in depth. Each surface validates with its own origin before
        // reaching here - the config loader on parse, the admin route before
        // it writes - so this pass uses the permissive origin: it re-checks the
        // rules both surfaces share, and must not re-apply the API-only
        // `LUMEN_` prefix rule to a perfectly legal file-declared variable.
        settings.validate(SettingsOrigin::ConfigFile)?;
        // Held across the whole sequence: validate, register, build, swap.
        let _guard = self
            .apply_lock
            .lock()
            .map_err(|_| "the webhook apply lock is poisoned".to_owned())?;
        let signing = self.resolve_signing_key(settings)?;
        let metrics = self.metrics_handle()?;

        let current = self.live.load_full();
        let reuse = current
            .as_ref()
            .filter(|p| p.settings.channel_capacity == settings.channel_capacity);
        let pipeline = if let Some(existing) = reuse {
            // Same capacity: retune in place. The sender task reads both cells
            // per attempt, so this takes effect on the next one.
            existing
                .policy
                .store(Arc::new(DeliveryPolicy::from(settings)));
            existing.signing.store(signing);
            Arc::new(Pipeline {
                settings: settings.clone(),
                queue: Arc::clone(&existing.queue),
                policy: Arc::clone(&existing.policy),
                signing: Arc::clone(&existing.signing),
            })
        } else {
            // A new capacity means a new queue and a new sender. The previous
            // sender keeps its receiver and exits once this function drops the
            // last clone of the old queue, after draining what it already had.
            let (queue, receiver) = SignalQueue::new(
                settings.channel_capacity,
                Arc::new(QueueCounters(metrics.clone())),
            );
            let policy = Arc::new(ArcSwap::from_pointee(DeliveryPolicy::from(settings)));
            let signing_cell = Arc::new(ArcSwapOption::empty());
            signing_cell.store(signing);
            spawn_webhook_sender(
                receiver,
                self.client.clone(),
                Arc::clone(&policy),
                Arc::clone(&signing_cell),
                metrics,
                self.cancel.clone(),
            );
            Arc::new(Pipeline {
                settings: settings.clone(),
                queue,
                policy,
                signing: signing_cell,
            })
        };
        // Installing the policy also re-arms every loaded key and group, so a
        // subject already past a threshold does not re-fire it.
        keys.set_signals(Some(Arc::new(BudgetSignals::new(
            Arc::clone(&pipeline.queue),
            &settings.events,
            &settings.thresholds,
        ))));
        self.live.store(Some(pipeline));
        Ok(())
    }

    /// Stop emitting events. The queue's last sender goes with the pipeline,
    /// so the sender task delivers whatever was already queued and then exits.
    pub fn disable(&self, keys: &AuthState) {
        // Best effort: a poisoned lock must not leave webhooks stuck on.
        let _guard = self.apply_lock.lock();
        keys.set_signals(None);
        self.live.store(None);
    }

    /// Register the webhook collectors on first use and reuse the handle
    /// afterwards. Must be called with `apply_lock` held: Prometheus refuses a
    /// duplicate registration, so two racing first-enables would make one of
    /// them fail for no operator-visible reason.
    fn metrics_handle(&self) -> Result<WebhookMetrics, String> {
        if let Some(metrics) = self.metrics.get() {
            return Ok(metrics.clone());
        }
        let metrics = WebhookMetrics::register(&self.registry)
            .map_err(|error| format!("could not register the webhook metrics: {error}"))?;
        let _ = self.metrics.set(metrics.clone());
        Ok(metrics)
    }

    /// Resolve the signing secret: the named environment variable first, then
    /// the sealed-at-rest secret (ADR 011 amendment §4).
    ///
    /// # Errors
    /// `signing_key_env` names a variable that is unset or empty and no secret
    /// is stored either. That is a broken deployment, not a choice: it would
    /// silently downgrade a billing integration to unsigned deliveries.
    fn resolve_signing_key(
        &self,
        settings: &WebhookSettings,
    ) -> Result<Option<Arc<SigningKey>>, String> {
        // Only variable NAMES appear in any message or log line below.
        if let Some(var) = &settings.signing_key_env {
            match std::env::var(var) {
                Ok(value) if !value.is_empty() => {
                    return Ok(Some(Arc::new(SigningKey::new(value.into_bytes()))));
                }
                _ => {
                    if let Some(stored) = self.stored_secret.load_full() {
                        tracing::warn!(
                            "webhooks.signing_key_env names '{var}', which is unset or empty; \
                             using the secret stored through PUT /admin/webhooks/signing-key"
                        );
                        return Ok(Some(stored));
                    }
                    return Err(format!(
                        "webhooks.signing_key_env names '{var}', which is unset or empty in the \
                         environment, and no signing secret is stored"
                    ));
                }
            }
        }
        if let Some(stored) = self.stored_secret.load_full() {
            return Ok(Some(stored));
        }
        tracing::warn!(
            "webhooks are enabled with no signing secret: deliveries carry no x-lumen-signature \
             header and the receiver cannot verify authenticity"
        );
        Ok(None)
    }
}

/// Bridges the queue's drop accounting to Prometheus, so `lumen_auth` never
/// has to know the telemetry crate exists.
#[derive(Debug)]
struct QueueCounters(WebhookMetrics);

impl SignalCounters for QueueCounters {
    fn inc_queued(&self) {
        self.0.inc_queued();
    }
    fn inc_dropped(&self) {
        self.0.inc_dropped();
    }
}

/// Spawn the delivery task. It owns `rx`, so it exits when the queue's senders
/// are all dropped or when `cancel` fires - whichever comes first.
///
/// Shutdown abandons whatever is still queued rather than blocking on it: the
/// events are a convenience signal and the reconciliation pull covers the gap
/// (ADR 011 §2).
pub fn spawn_webhook_sender(
    mut rx: Receiver<BudgetEvent>,
    client: reqwest::Client,
    policy: Arc<ArcSwap<DeliveryPolicy>>,
    signing: Arc<ArcSwapOption<SigningKey>>,
    metrics: WebhookMetrics,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let event = tokio::select! {
                // `biased` so a pending cancellation always wins over a
                // backlog: shutdown must not be delayed by a full queue.
                biased;
                () = cancel.cancelled() => break,
                received = rx.recv() => match received {
                    Some(event) => event,
                    None => break,
                },
            };
            deliver(&client, &policy, &signing, &metrics, &cancel, event).await;
        }
        tracing::debug!("webhook sender stopped");
    })
}

/// Whether an HTTP status is worth another attempt. 5xx, 429 and 408 are
/// transient; every other non-2xx is the receiver telling us this event will
/// never be accepted, so retrying it only wastes the retry budget of the
/// events behind it.
fn is_retryable(status: reqwest::StatusCode) -> bool {
    status.is_server_error()
        || status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || status == reqwest::StatusCode::REQUEST_TIMEOUT
}

/// `base * 2^failed`, capped, with equal jitter: `d ∈ [exp/2, exp]`. Mirrors
/// the router's retry backoff (M6 §6.1) rather than inventing a second curve.
fn backoff(base: Duration, failed: u32, rand01: f64) -> Duration {
    /// Ceiling on the exponential term, so a long-lived outage settles into a
    /// steady poll instead of an ever-growing wait.
    const MAX: Duration = Duration::from_secs(30);
    let exp_ms = base
        .as_millis()
        .saturating_mul(1_u128 << failed.min(20))
        .min(MAX.as_millis());
    let exp_ms = u64::try_from(exp_ms).unwrap_or(u64::MAX);
    let half = exp_ms / 2;
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )] // `rand01` is clamped to [0, 1], so the product is non-negative.
    let jitter = (half as f64 * rand01.clamp(0.0, 1.0)) as u64;
    Duration::from_millis(half.saturating_add(jitter).max(1))
}

/// A lock-free pseudo-random fraction in `[0, 1)` (splitmix64), for backoff
/// jitter. Not for cryptographic use - the signing key is the crypto here.
fn jitter01() -> f64 {
    const GAMMA: u64 = 0x9E37_79B9_7F4A_7C15;
    static STATE: AtomicU64 = AtomicU64::new(GAMMA);
    let seed = STATE
        .fetch_add(GAMMA, Ordering::Relaxed)
        .wrapping_add(GAMMA);
    let mut z = seed;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    #[allow(clippy::cast_precision_loss)]
    {
        (z >> 11) as f64 / (1_u64 << 53) as f64
    }
}

/// Deliver one event, retrying transient failures until it lands, the receiver
/// rejects it permanently, the retry budget runs out, or shutdown cancels.
async fn deliver(
    client: &reqwest::Client,
    policy: &ArcSwap<DeliveryPolicy>,
    signing: &ArcSwapOption<SigningKey>,
    metrics: &WebhookMetrics,
    cancel: &CancellationToken,
    event: BudgetEvent,
) {
    let body = match serde_json::to_vec(&event) {
        Ok(body) => body,
        Err(error) => {
            // Unreachable for this payload shape (plain scalars and strings),
            // but a serialization bug must not take the task down.
            tracing::warn!(%error, event_id = %event.id, "webhook payload could not be serialized");
            metrics.inc_dead();
            return;
        }
    };
    // Read the key cell once per event, not per attempt: every attempt must
    // carry the same signature as the body it is retrying, and a rotation
    // mid-retry would otherwise make the retries unverifiable.
    let signature = signing.load().as_ref().map(|key| key.sign(&body));

    for attempt in 0..policy.load().max_attempts {
        if cancel.is_cancelled() {
            tracing::debug!(event_id = %event.id, "webhook delivery abandoned at shutdown");
            return;
        }
        // Re-read per attempt so a hot reload retargets a retry in flight.
        let current = policy.load_full();
        let mut request = client
            .post(&current.url)
            .timeout(current.timeout)
            .header("content-type", "application/json")
            .header(EVENT_ID_HEADER, &event.id)
            .header(EVENT_HEADER, event.event.as_str())
            .header(TIMESTAMP_HEADER, event.ts.to_string())
            .body(body.clone());
        if let Some(signature) = &signature {
            request = request.header(SIGNATURE_HEADER, signature);
        }

        let started = Instant::now();
        let outcome = tokio::select! {
            biased;
            () = cancel.cancelled() => {
                tracing::debug!(event_id = %event.id, "webhook delivery cancelled in flight");
                return;
            }
            result = request.send() => result,
        };
        metrics.observe_delivery(started.elapsed().as_secs_f64());

        let retryable = match outcome {
            Ok(response) if response.status().is_success() => {
                metrics.inc_sent();
                tracing::debug!(
                    event_id = %event.id,
                    event = %event.event,
                    attempt = attempt + 1,
                    "webhook delivered"
                );
                return;
            }
            Ok(response) => {
                let status = response.status();
                let retryable = is_retryable(status);
                if !retryable {
                    // A permanent rejection: log the status, never the body
                    // (a receiver's error page is not ours to record).
                    tracing::warn!(
                        event_id = %event.id,
                        status = status.as_u16(),
                        "webhook receiver rejected the event permanently"
                    );
                }
                retryable
            }
            Err(error) => {
                tracing::debug!(
                    event_id = %event.id,
                    %error,
                    attempt = attempt + 1,
                    "webhook delivery attempt failed"
                );
                true
            }
        };

        let attempts_left = attempt + 1 < current.max_attempts;
        if !retryable || !attempts_left {
            break;
        }
        metrics.inc_retry();
        let wait = backoff(current.retry_base, attempt, jitter01());
        tokio::select! {
            biased;
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(wait) => {}
        }
    }

    metrics.inc_dead();
    tracing::warn!(
        event_id = %event.id,
        event = %event.event,
        "webhook event abandoned; reconcile through GET /admin/usage/export"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_signing_key_is_redacted_in_debug_output() {
        let key = SigningKey::new("super-secret-webhook-key");
        let rendered = format!("{key:?}");
        assert_eq!(rendered, "SigningKey(REDACTED)");
        assert!(!rendered.contains("super-secret"));
    }

    #[test]
    fn signatures_match_the_hmac_sha256_of_the_body() {
        // RFC 4231 test case 1: key = 20 * 0x0b, data = "Hi There".
        let key = SigningKey::new(vec![0x0b; 20]);
        assert_eq!(
            key.sign(b"Hi There"),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    #[test]
    fn a_different_body_or_key_produces_a_different_signature() {
        let key = SigningKey::new("k1");
        let other = SigningKey::new("k2");
        assert_ne!(key.sign(b"{}"), key.sign(b"{ }"));
        assert_ne!(key.sign(b"{}"), other.sign(b"{}"));
    }

    #[test]
    fn only_transient_statuses_are_retried() {
        use reqwest::StatusCode;
        for status in [
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::BAD_GATEWAY,
            StatusCode::SERVICE_UNAVAILABLE,
            StatusCode::TOO_MANY_REQUESTS,
            StatusCode::REQUEST_TIMEOUT,
        ] {
            assert!(is_retryable(status), "{status} should be retried");
        }
        for status in [
            StatusCode::BAD_REQUEST,
            StatusCode::UNAUTHORIZED,
            StatusCode::FORBIDDEN,
            StatusCode::NOT_FOUND,
            StatusCode::UNPROCESSABLE_ENTITY,
        ] {
            assert!(!is_retryable(status), "{status} should not be retried");
        }
    }

    #[test]
    fn backoff_is_exponential_within_its_equal_jitter_bounds() {
        let base = Duration::from_millis(500);
        // rand01 = 0 -> the lower bound exp/2; rand01 -> 1 -> the bound exp.
        assert_eq!(backoff(base, 0, 0.0), Duration::from_millis(250));
        assert_eq!(backoff(base, 0, 1.0), Duration::from_millis(500));
        assert_eq!(backoff(base, 1, 0.0), Duration::from_millis(500));
        assert_eq!(backoff(base, 2, 0.0), Duration::from_millis(1_000));
        // The 30 s ceiling applies before jitter, so a huge exponent settles.
        assert_eq!(backoff(base, 30, 0.0), Duration::from_millis(15_000));
        assert_eq!(backoff(base, 30, 1.0), Duration::from_millis(30_000));
        // Never zero: a 1 ms base still waits.
        assert!(backoff(Duration::from_millis(1), 0, 0.0) >= Duration::from_millis(1));
    }

    #[test]
    fn jitter_stays_inside_the_unit_interval() {
        for _ in 0..1_000 {
            let r = jitter01();
            assert!((0.0..1.0).contains(&r), "jitter out of range: {r}");
        }
    }

    /// An inert controller with no HTTP client work to do - enough to
    /// exercise the signing-key resolution rules on their own.
    fn controller() -> WebhookController {
        WebhookController::new(
            Metrics::new(),
            reqwest::Client::new(),
            CancellationToken::new(),
        )
    }

    fn settings_with_env(var: Option<&str>) -> WebhookSettings {
        WebhookSettings {
            url: "https://example.test/hook".to_owned(),
            signing_key_env: var.map(str::to_owned),
            ..WebhookSettings::default()
        }
    }

    #[test]
    fn a_named_but_unset_signing_env_var_is_an_error_naming_only_the_variable() {
        let error = controller()
            .resolve_signing_key(&settings_with_env(Some("LUMEN_TEST_WEBHOOK_SECRET_ABSENT")))
            .expect_err("an unset variable with no stored secret must fail");
        assert!(
            error.contains("LUMEN_TEST_WEBHOOK_SECRET_ABSENT"),
            "{error}"
        );
        assert!(error.contains("no signing secret is stored"), "{error}");
    }

    #[test]
    fn an_absent_signing_env_var_and_no_stored_secret_yields_unsigned_delivery() {
        assert!(controller()
            .resolve_signing_key(&settings_with_env(None))
            .expect("no secret configured at all is allowed")
            .is_none());
    }

    #[test]
    fn the_stored_secret_fills_in_for_an_unset_env_var() {
        // ADR 011 amendment §4: the environment is primary, the sealed secret
        // fills in. A named-but-unset variable is only fatal when nothing
        // else can sign - otherwise deliveries stay signed, which is the
        // property that actually matters.
        let controller = controller();
        controller
            .stored_secret
            .store(Some(Arc::new(SigningKey::new("stored-secret"))));
        let resolved = controller
            .resolve_signing_key(&settings_with_env(Some("LUMEN_TEST_WEBHOOK_ABSENT_2")))
            .expect("the stored secret fills in")
            .expect("some key");
        assert_eq!(
            resolved.sign(b"body"),
            SigningKey::new("stored-secret").sign(b"body")
        );

        // With no variable named at all, the stored secret is used directly.
        let resolved = controller
            .resolve_signing_key(&settings_with_env(None))
            .expect("stored secret")
            .expect("some key");
        assert_eq!(
            resolved.sign(b"body"),
            SigningKey::new("stored-secret").sign(b"body")
        );
    }

    #[test]
    fn the_environment_wins_over_the_stored_secret() {
        // Safe on edition 2021; the variable is scoped to this test's name.
        std::env::set_var("LUMEN_TEST_WEBHOOK_ENV_WINS", "env-secret");
        let controller = controller();
        controller
            .stored_secret
            .store(Some(Arc::new(SigningKey::new("stored-secret"))));
        let resolved = controller
            .resolve_signing_key(&settings_with_env(Some("LUMEN_TEST_WEBHOOK_ENV_WINS")))
            .expect("resolves")
            .expect("some key");
        assert_eq!(
            resolved.sign(b"body"),
            SigningKey::new("env-secret").sign(b"body")
        );
        std::env::remove_var("LUMEN_TEST_WEBHOOK_ENV_WINS");
    }

    /// A `MakeWriter` that appends everything into a shared buffer, so a test
    /// can inspect exactly what was logged.
    #[derive(Clone)]
    struct BufMakeWriter(Arc<std::sync::Mutex<Vec<u8>>>);

    struct BufGuard(Arc<std::sync::Mutex<Vec<u8>>>);

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for BufMakeWriter {
        type Writer = BufGuard;
        fn make_writer(&'a self) -> Self::Writer {
            BufGuard(Arc::clone(&self.0))
        }
    }

    impl std::io::Write for BufGuard {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .map_err(|_| std::io::Error::other("log buffer poisoned"))?
                .extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A failing delivery logs at every level it has - and must never put the
    /// signing secret, or its HMAC-relevant material, into any of those lines.
    #[tokio::test]
    async fn a_failing_delivery_never_logs_the_signing_secret() {
        const SECRET: &str = "whsec-DO-NOT-LOG-THIS-VALUE";
        let buffer = Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(BufMakeWriter(Arc::clone(&buffer)))
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .finish();

        // Port 0 on the loopback never accepts, so every attempt fails fast
        // and the event is abandoned - the noisiest path there is.
        let policy = ArcSwap::from_pointee(DeliveryPolicy {
            url: "http://127.0.0.1:0/events".to_owned(),
            timeout: Duration::from_millis(50),
            max_attempts: 2,
            retry_base: Duration::from_millis(1),
        });
        let signing = ArcSwapOption::from_pointee(SigningKey::new(SECRET));
        let metrics = WebhookMetrics::register(&Metrics::new()).expect("register webhook metrics");
        let event = BudgetEvent {
            id: "evt_test".to_owned(),
            event: lumen_auth::events::EventKind::BudgetThreshold,
            scope: lumen_auth::events::EventScope::Key,
            subject_id: "k1".to_owned(),
            subject_name: "prepaid".to_owned(),
            budget_max: Some(100.0),
            budget_spent: 80.0,
            threshold: Some(80),
            ts: 1_700_000_000,
        };

        let guard = tracing::subscriber::set_default(subscriber);
        deliver(
            &reqwest::Client::new(),
            &policy,
            &signing,
            &metrics,
            &CancellationToken::new(),
            event,
        )
        .await;
        drop(guard);

        let logs =
            String::from_utf8(buffer.lock().expect("log buffer").clone()).expect("logs are utf-8");
        assert!(!logs.is_empty(), "the failing delivery should have logged");
        assert!(
            !logs.contains(SECRET),
            "signing secret leaked into logs: {logs}"
        );
        assert!(
            !logs.contains("SigningKey("),
            "the key must not be rendered at all: {logs}"
        );
        // The event itself is safe to name, and is what an operator needs.
        assert!(logs.contains("evt_test"), "{logs}");
    }

    #[test]
    fn an_empty_signing_env_var_is_rejected_without_echoing_its_value() {
        // Safe on edition 2021; the variable is scoped to this test's name.
        std::env::set_var("LUMEN_TEST_WEBHOOK_SECRET_EMPTY", "");
        let error = controller()
            .resolve_signing_key(&settings_with_env(Some("LUMEN_TEST_WEBHOOK_SECRET_EMPTY")))
            .expect_err("an empty secret must fail");
        assert!(error.contains("LUMEN_TEST_WEBHOOK_SECRET_EMPTY"));
        assert!(error.contains("unset or empty"));
        std::env::remove_var("LUMEN_TEST_WEBHOOK_SECRET_EMPTY");
    }

    #[test]
    fn the_delivery_policy_mirrors_the_settings() {
        let config = WebhookSettings {
            url: "https://example.test/hook".to_owned(),
            timeout_ms: 1_500,
            max_attempts: 3,
            retry_base_ms: 250,
            ..WebhookSettings::default()
        };
        let policy = DeliveryPolicy::from(&config);
        assert_eq!(policy.url, "https://example.test/hook");
        assert_eq!(policy.timeout, Duration::from_millis(1_500));
        assert_eq!(policy.max_attempts, 3);
        assert_eq!(policy.retry_base, Duration::from_millis(250));
    }
}
