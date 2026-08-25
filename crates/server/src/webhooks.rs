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
//! The signing secret is read from the environment once at boot (a running
//! process cannot see a changed env var) and is wrapped in [`SigningKey`],
//! whose `Debug` is redacted and whose bytes are zeroized on drop, so it can
//! never reach a log line or an error message.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use hmac::{Hmac, KeyInit, Mac};
use lumen_auth::events::{BudgetEvent, BudgetSignals, SignalCounters, SignalQueue};
use lumen_telemetry::WebhookMetrics;
use sha2::Sha256;
use tokio::sync::mpsc::Receiver;
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use crate::config::WebhooksConfig;

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

impl DeliveryPolicy {
    /// Derive the policy from a validated `[webhooks]` block.
    #[must_use]
    pub fn from_config(config: &WebhooksConfig) -> Self {
        Self {
            url: config.url.clone(),
            timeout: Duration::from_millis(config.timeout_ms),
            max_attempts: config.max_attempts.max(1),
            retry_base: Duration::from_millis(config.retry_base_ms),
        }
    }
}

/// The process-wide webhook runtime: the bounded queue (built once) and the
/// live delivery policy (swapped by a hot reload). Held by `main` so it can be
/// handed to both the reloader and the sender task.
#[derive(Debug)]
pub struct WebhookRuntime {
    queue: Arc<SignalQueue>,
    policy: Arc<ArcSwap<DeliveryPolicy>>,
    /// The queue capacity and signing-key variable this process booted with.
    /// Both are structural (the channel is built once; a running process
    /// cannot observe a changed env var), so a reload that alters them is
    /// reported rather than silently ignored.
    boot_capacity: usize,
    boot_signing_key_env: Option<String>,
}

impl WebhookRuntime {
    /// Build the runtime from a validated `[webhooks]` block: create the
    /// bounded queue, the initial policy and the initial signalling policy.
    ///
    /// Returns the runtime, the receiver the sender task drains, and the
    /// [`BudgetSignals`] to install on the auth state.
    #[must_use]
    pub fn build(
        config: &WebhooksConfig,
        metrics: WebhookMetrics,
    ) -> (Arc<Self>, Receiver<BudgetEvent>, Arc<BudgetSignals>) {
        let (queue, rx) = SignalQueue::new(
            config.channel_capacity,
            Arc::new(QueueCounters::new(metrics)),
        );
        let runtime = Arc::new(Self {
            queue: Arc::clone(&queue),
            policy: Arc::new(ArcSwap::from_pointee(DeliveryPolicy::from_config(config))),
            boot_capacity: config.channel_capacity,
            boot_signing_key_env: config.signing_key_env.clone(),
        });
        let signals = runtime.signals_from(config);
        (runtime, rx, signals)
    }

    /// Build a signalling policy over this runtime's queue. Used at boot and
    /// on every reload, so the sender task always keeps its receiver.
    #[must_use]
    pub fn signals_from(&self, config: &WebhooksConfig) -> Arc<BudgetSignals> {
        Arc::new(BudgetSignals::new(
            Arc::clone(&self.queue),
            &config.events,
            &config.thresholds,
        ))
    }

    /// Swap the delivery policy (hot reload). The sender task reads the cell
    /// per attempt, so the next attempt uses the new URL and timeouts.
    pub fn set_policy(&self, policy: DeliveryPolicy) {
        self.policy.store(Arc::new(policy));
    }

    /// The policy cell the sender task reads.
    #[must_use]
    pub fn policy(&self) -> Arc<ArcSwap<DeliveryPolicy>> {
        Arc::clone(&self.policy)
    }

    /// Apply a reloaded config to this runtime and return the signalling
    /// policy to install on the auth state (`None` = the `[webhooks]` block
    /// was removed, so detection stops and no further event is queued).
    ///
    /// The queue and the sender task are untouched, so a reload can retarget
    /// and retune delivery without dropping whatever is already queued. The
    /// two structural knobs are reported instead of applied.
    pub fn apply_reload(&self, block: Option<&WebhooksConfig>) -> Option<Arc<BudgetSignals>> {
        let Some(block) = block else {
            tracing::info!(
                "[webhooks] removed from the config; budget events are no longer emitted"
            );
            return None;
        };
        if block.channel_capacity != self.boot_capacity {
            tracing::warn!(
                configured = block.channel_capacity,
                in_effect = self.boot_capacity,
                "webhooks.channel_capacity is structural; the bounded queue keeps its boot size                  until a restart"
            );
        }
        if block.signing_key_env != self.boot_signing_key_env {
            tracing::warn!(
                "webhooks.signing_key_env changed; the signing secret is read from the                  environment at boot and keeps its boot value until a restart"
            );
        }
        self.set_policy(DeliveryPolicy::from_config(block));
        Some(self.signals_from(block))
    }
}

/// Bridges the queue's drop accounting to Prometheus, so `lumen_auth` never
/// has to know the telemetry crate exists.
#[derive(Debug)]
struct QueueCounters(WebhookMetrics);

impl QueueCounters {
    fn new(metrics: WebhookMetrics) -> Self {
        Self(metrics)
    }
}

impl SignalCounters for QueueCounters {
    fn inc_queued(&self) {
        self.0.inc_queued();
    }
    fn inc_dropped(&self) {
        self.0.inc_dropped();
    }
}

/// Read the signing secret named by `signing_key_env`.
///
/// A named-but-unset (or empty) variable is an operator mistake worth failing
/// on: silently delivering unsigned billing events would be a security
/// downgrade nobody asked for. An absent `signing_key_env` is a deliberate
/// choice and only warns.
///
/// # Errors
/// The env var is named in config but missing or empty in the environment.
pub fn load_signing_key(config: &WebhooksConfig) -> Result<Option<SigningKey>, String> {
    let Some(var) = &config.signing_key_env else {
        tracing::warn!(
            "webhooks are enabled without signing_key_env: deliveries carry no \
             x-lumen-signature header and the receiver cannot verify authenticity"
        );
        return Ok(None);
    };
    // Only the variable NAME is ever logged or returned in an error.
    match std::env::var(var) {
        Ok(value) if !value.is_empty() => Ok(Some(SigningKey::new(value.into_bytes()))),
        _ => Err(format!(
            "webhooks.signing_key_env names '{var}', which is unset or empty in the environment"
        )),
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
    signing_key: Option<SigningKey>,
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
            deliver(
                &client,
                &policy,
                signing_key.as_ref(),
                &metrics,
                &cancel,
                event,
            )
            .await;
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
    signing_key: Option<&SigningKey>,
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
    let signature = signing_key.map(|key| key.sign(&body));

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

    #[test]
    fn a_named_but_unset_signing_env_var_is_an_error_naming_only_the_variable() {
        let config = WebhooksConfig {
            url: "https://example.test/hook".to_owned(),
            signing_key_env: Some("LUMEN_TEST_WEBHOOK_SECRET_ABSENT".to_owned()),
            ..WebhooksConfig::default()
        };
        let error = load_signing_key(&config).expect_err("unset var must fail");
        assert!(error.contains("LUMEN_TEST_WEBHOOK_SECRET_ABSENT"));
    }

    #[test]
    fn an_absent_signing_env_var_yields_unsigned_delivery() {
        let config = WebhooksConfig {
            url: "https://example.test/hook".to_owned(),
            signing_key_env: None,
            ..WebhooksConfig::default()
        };
        assert!(load_signing_key(&config)
            .expect("no env var configured is allowed")
            .is_none());
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
        let key = SigningKey::new(SECRET);
        let metrics = WebhookMetrics::register(&lumen_telemetry::Metrics::new())
            .expect("register webhook metrics");
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
            Some(&key),
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
        let config = WebhooksConfig {
            url: "https://example.test/hook".to_owned(),
            signing_key_env: Some("LUMEN_TEST_WEBHOOK_SECRET_EMPTY".to_owned()),
            ..WebhooksConfig::default()
        };
        let error = load_signing_key(&config).expect_err("an empty secret must fail");
        assert!(error.contains("LUMEN_TEST_WEBHOOK_SECRET_EMPTY"));
        assert!(error.contains("unset or empty"));
        std::env::remove_var("LUMEN_TEST_WEBHOOK_SECRET_EMPTY");
    }

    #[test]
    fn the_delivery_policy_mirrors_the_config_block() {
        let config = WebhooksConfig {
            url: "https://example.test/hook".to_owned(),
            timeout_ms: 1_500,
            max_attempts: 3,
            retry_base_ms: 250,
            ..WebhooksConfig::default()
        };
        let policy = DeliveryPolicy::from_config(&config);
        assert_eq!(policy.url, "https://example.test/hook");
        assert_eq!(policy.timeout, Duration::from_millis(1_500));
        assert_eq!(policy.max_attempts, 3);
        assert_eq!(policy.retry_base, Duration::from_millis(250));
    }
}
