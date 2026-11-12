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
//! * the signing secret lives in a cell the sender reads once per **event**,
//!   so a rotation applies to the next delivery without restarting anything -
//!   while every attempt at an event already in flight keeps the signature its
//!   body was signed with, which is what makes a retry verifiable.

use std::fmt;
use std::net::IpAddr;
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
use lumen_providers::image_fetch::{host_allowed, is_public_ip};
use lumen_telemetry::{Metrics, WebhookMetrics};
use sha2::Sha256;
use tokio::sync::mpsc::Receiver;
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

/// Environment variable listing the internal hosts an admin-supplied webhook
/// receiver may name (comma-separated; `.suffix` entries match subdomains).
/// Read once at boot; see [`WebhookController::check_receiver_url`].
pub const ALLOWED_HOSTS_ENV: &str = "LUMEN_WEBHOOK_ALLOWED_HOSTS";

/// How long the receiver guard waits for a hostname to resolve.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(3);

/// Parse an [`ALLOWED_HOSTS_ENV`] value: comma-separated, each entry trimmed
/// and normalised like the hosts it is compared with (IPv6 brackets and a
/// trailing dot dropped, lowercased); empty entries are skipped.
#[must_use]
pub fn parse_receiver_allowlist(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(normalise_host)
        .filter(|h| !h.is_empty() && h != ".")
        .collect()
}

/// The comparable form of a host: no IPv6 brackets, no trailing dot,
/// lowercase.
fn normalise_host(host: &str) -> String {
    host.trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.')
        .to_ascii_lowercase()
}

/// Resolve `host:port` without blocking the runtime, bounded by
/// [`RESOLVE_TIMEOUT`].
async fn resolve_host(host: String, port: u16) -> Result<Vec<IpAddr>, String> {
    match tokio::time::timeout(RESOLVE_TIMEOUT, tokio::net::lookup_host((host, port))).await {
        Ok(Ok(addrs)) => Ok(addrs.map(|a| a.ip()).collect()),
        Ok(Err(_)) => Err("could not be resolved".to_owned()),
        Err(_) => Err("did not resolve in time".to_owned()),
    }
}

/// [`WebhookController::check_receiver_url`], with the resolver injected so
/// the decision is testable without DNS.
async fn check_receiver_url<F, Fut>(
    url: &str,
    allowlist: &[String],
    resolve: F,
) -> Result<(), String>
where
    F: FnOnce(String, u16) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<IpAddr>, String>>,
{
    let refuse = |host: &str, why: &str| {
        format!(
            "webhooks.url host '{host}' {why}; receivers set through the admin API must be \
             publicly routable unless the operator lists the host in {ALLOWED_HOSTS_ENV}"
        )
    };
    let parsed =
        reqwest::Url::parse(url).map_err(|e| format!("webhooks.url is not a valid URL: {e}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err("webhooks.url must be an http:// or https:// URL".to_owned());
    }
    // The WHATWG parser already canonicalises every IPv4 spelling
    // (`2130706433`, `0x7f.0.0.1`) to dotted form and lowercases names;
    // IPv6 literals keep their brackets in `host_str`. `raw` is what delivery
    // will resolve; `host` is the comparable form for the checks.
    let raw = parsed
        .host_str()
        .ok_or_else(|| "webhooks.url must name a host".to_owned())?;
    let host = normalise_host(raw);
    if !allowlist.is_empty() && host_allowed(&host, allowlist) {
        return Ok(());
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        return if is_public_ip(&ip) {
            Ok(())
        } else {
            Err(refuse(
                &host,
                "is a loopback, private, link-local or otherwise internal address",
            ))
        };
    }
    if host == "localhost" || host.ends_with(".localhost") {
        return Err(refuse(&host, "is a loopback name"));
    }
    let port = parsed.port_or_known_default().unwrap_or(443);
    let addrs = resolve(raw.to_owned(), port)
        .await
        .map_err(|why| refuse(&host, &why))?;
    if addrs.is_empty() {
        return Err(refuse(&host, "could not be resolved"));
    }
    if addrs.iter().any(|ip| !is_public_ip(ip)) {
        return Err(refuse(&host, "resolves to an internal address"));
    }
    Ok(())
}

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
    pub(crate) fn sign(&self, body: &[u8]) -> String {
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

/// Why webhook settings could not be applied (or would not be, for a
/// [`preflight`](WebhookController::preflight)). The two cases are different
/// faults: the admin API answers the first with a 400 and the second with a
/// 500.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WebhookApplyError {
    /// The settings themselves are at fault: they do not validate, or name a
    /// signing secret this process cannot resolve. The message names the
    /// field or the variable, never a value.
    #[error("{0}")]
    Invalid(String),
    /// The gateway is at fault whatever the settings say: the Prometheus
    /// collectors could not be registered, or the apply lock is poisoned.
    #[error("{0}")]
    Internal(String),
}

impl WebhookApplyError {
    fn lock_poisoned() -> Self {
        Self::Internal("the webhook apply lock is poisoned".to_owned())
    }
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
    /// Serialises [`refresh_from_store`](Self::refresh_from_store): held
    /// across the database read and the cache store, so the cache only ever
    /// moves forward. Without it a reload's refresh could read the row, lose
    /// the race to an admin write's own refresh, and then overwrite the
    /// fresher cached row with its older read.
    refresh_lock: tokio::sync::Mutex<()>,
    /// Hosts an admin-supplied receiver URL may name even though they are
    /// internal ([`ALLOWED_HOSTS_ENV`]); empty means public receivers only.
    receiver_allowlist: Vec<String>,
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
            refresh_lock: tokio::sync::Mutex::new(()),
            receiver_allowlist: Vec::new(),
        }
    }

    /// Let admin-supplied receiver URLs name these internal hosts (builder
    /// style). Entries match like `image_fetch.allowed_hosts`: exactly, or as
    /// a domain and its subdomains when written `.suffix`; case-insensitive.
    /// `main.rs` fills it from [`ALLOWED_HOSTS_ENV`].
    #[must_use]
    pub fn with_receiver_allowlist(mut self, hosts: Vec<String>) -> Self {
        self.receiver_allowlist = hosts;
        self
    }

    /// The SSRF guard for a receiver URL set through `PUT /admin/webhooks`.
    ///
    /// The admin API decides where the gateway POSTs, so a receiver must be
    /// an `http(s)` URL whose host is publicly routable: loopback, private,
    /// link-local (cloud metadata), unique-local, shared and other reserved
    /// addresses are refused, as is any `localhost` name, and a hostname is
    /// resolved and refused when *any* of its addresses is internal. A host
    /// on the operator allowlist skips the address checks (and is never
    /// resolved). Also run by the `/admin/config*` writes when they introduce
    /// or change the `[webhooks]` receiver; a receiver already in the document
    /// (the operator's own file) and stored rows written before this check
    /// existed are not re-checked.
    ///
    /// This is an admission check: it does not pin the address used at
    /// delivery time, so a hostname re-pointed afterwards (DNS rebinding) or
    /// a receiver answering with a redirect is outside its reach.
    ///
    /// # Errors
    /// A message naming `webhooks.url` and the allowlist variable.
    pub async fn check_receiver_url(&self, url: &str) -> Result<(), String> {
        check_receiver_url(url, &self.receiver_allowlist, resolve_host).await
    }

    /// Refresh the cached database view: the stored settings row and the
    /// sealed signing secret.
    ///
    /// Runs at boot, in the reload task, and after an admin write - never on a
    /// request path. A failure leaves the previous cache in place and is
    /// logged, so a sick database cannot strip a working configuration.
    ///
    /// Refreshes are serialised (read and store under one guard): an admin
    /// write refreshes after its own database write, so once that refresh
    /// returns no refresh that read earlier can still replace the cache.
    pub async fn refresh_from_store(&self, store: &KeyStore, master: Option<&MasterKey>) {
        let _refresh = self.refresh_lock.lock().await;
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
    /// [`WebhookApplyError::Invalid`] when the winning settings do not
    /// validate or the signing secret cannot be resolved,
    /// [`WebhookApplyError::Internal`] when the Prometheus collectors cannot
    /// be registered or the apply lock is poisoned. The previous pipeline is
    /// left running in every case.
    pub fn resolve_and_apply(
        &self,
        file_block: Option<&WebhookSettings>,
        keys: &AuthState,
    ) -> Result<SettingsSource, WebhookApplyError> {
        // The cached row is read UNDER the apply lock: reading it first would
        // let an admin write refresh the cache and apply its settings between
        // this read and this apply, which would then put the older row back.
        // A poisoned lock still excludes other appliers (the error carries
        // the guard); only the apply branches refuse it, like `apply`.
        let guard = self.apply_lock.lock();
        let poisoned = WebhookApplyError::lock_poisoned;
        match self.stored() {
            Some(stored) if stored.enabled => {
                if guard.is_err() {
                    return Err(poisoned());
                }
                self.apply_locked(&stored.settings, keys)?;
                Ok(SettingsSource::Database)
            }
            // A stored row marked disabled shadows the file block on purpose:
            // a DELETE must not be undone by the next reload.
            Some(_) => {
                self.disable_locked(keys);
                Ok(SettingsSource::None)
            }
            None => {
                if let Some(settings) = file_block {
                    if guard.is_err() {
                        return Err(poisoned());
                    }
                    self.apply_locked(settings, keys)?;
                    Ok(SettingsSource::Config)
                } else {
                    self.disable_locked(keys);
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
    /// [`WebhookApplyError::Invalid`] for invalid settings or an unresolvable
    /// signing secret, [`WebhookApplyError::Internal`] for a metrics
    /// registration failure or a poisoned apply lock. Nothing is swapped on
    /// error.
    pub fn apply(
        &self,
        settings: &WebhookSettings,
        keys: &AuthState,
    ) -> Result<(), WebhookApplyError> {
        // Held across the whole sequence: validate, register, build, swap.
        let _guard = self
            .apply_lock
            .lock()
            .map_err(|_| WebhookApplyError::lock_poisoned())?;
        self.apply_locked(settings, keys)
    }

    /// [`apply`](Self::apply) for a caller that already holds `apply_lock`.
    fn apply_locked(
        &self,
        settings: &WebhookSettings,
        keys: &AuthState,
    ) -> Result<(), WebhookApplyError> {
        // Defence in depth. Each surface validates with its own origin before
        // reaching here - the config loader on parse, the admin route before
        // it writes - so this pass uses the permissive origin: it re-checks the
        // rules both surfaces share, and must not re-apply the API-only
        // `LUMEN_` prefix rule to a perfectly legal file-declared variable.
        settings
            .validate(SettingsOrigin::ConfigFile)
            .map_err(WebhookApplyError::Invalid)?;
        let signing = self
            .resolve_signing_key(settings)
            .map_err(WebhookApplyError::Invalid)?;
        let metrics = self.metrics_handle()?;

        let current = self.live.load_full();
        let reuse = current
            .as_ref()
            .filter(|p| p.settings.channel_capacity == settings.channel_capacity);
        let pipeline = if let Some(existing) = reuse {
            // Same capacity: retune in place. The sender re-reads the policy
            // per attempt (so this retargets even a retry in flight) and the
            // key cell once per event (so a rotation applies to the next
            // event, never mid-retry).
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
        self.disable_locked(keys);
    }

    /// Turn the pipeline off after an admin write persisted a disabled row:
    /// refresh the cache from the store, then disable under the apply lock.
    ///
    /// The order matters, and is the one `PUT /admin/webhooks` uses. A reload
    /// that cached the row before the write resolves from the cache under the
    /// apply lock ([`resolve_and_apply`](Self::resolve_and_apply)): once this
    /// refresh has returned it can only see the disabled row, whichever side
    /// of the disable it runs on. Disabling first would leave a window in
    /// which that reload re-applies the stale enabled row for good.
    pub async fn disable_from_store(
        &self,
        store: &KeyStore,
        master: Option<&MasterKey>,
        keys: &AuthState,
    ) {
        self.refresh_from_store(store, master).await;
        self.disable(keys);
    }

    /// [`disable`](Self::disable) for a caller that already holds `apply_lock`.
    fn disable_locked(&self, keys: &AuthState) {
        keys.set_signals(None);
        self.live.store(None);
    }

    /// Register the webhook collectors on first use and reuse the handle
    /// afterwards. Must be called with `apply_lock` held: Prometheus refuses a
    /// duplicate registration, so two racing first-enables would make one of
    /// them fail for no operator-visible reason.
    fn metrics_handle(&self) -> Result<WebhookMetrics, WebhookApplyError> {
        if let Some(metrics) = self.metrics.get() {
            return Ok(metrics.clone());
        }
        let metrics = WebhookMetrics::register(&self.registry).map_err(|error| {
            WebhookApplyError::Internal(format!("could not register the webhook metrics: {error}"))
        })?;
        let _ = self.metrics.set(metrics.clone());
        Ok(metrics)
    }

    /// Run every fallible step of [`apply`](Self::apply) without building,
    /// swapping or logging anything: validation, signing-secret resolution
    /// and the metrics registration.
    ///
    /// The admin writes call this BEFORE they persist, so a document the
    /// gateway could not apply never replaces a stored one that works.
    ///
    /// # Errors
    /// The same errors [`apply`](Self::apply) would return: the document's
    /// own faults as [`WebhookApplyError::Invalid`], the gateway's as
    /// [`WebhookApplyError::Internal`].
    pub fn preflight(&self, settings: &WebhookSettings) -> Result<(), WebhookApplyError> {
        settings
            .validate(SettingsOrigin::ConfigFile)
            .map_err(WebhookApplyError::Invalid)?;
        self.check_signing_key(settings)
            .map_err(WebhookApplyError::Invalid)?;
        let _guard = self
            .apply_lock
            .lock()
            .map_err(|_| WebhookApplyError::lock_poisoned())?;
        self.metrics_handle().map(|_metrics| ())
    }

    /// Whether the signing secret `settings` asks for can be resolved right
    /// now (environment first, then the sealed secret). A dry run of the
    /// resolution [`apply`](Self::apply) performs: nothing is logged, nothing
    /// is swapped.
    ///
    /// # Errors
    /// `signing_key_env` names a variable that is unset or empty and no secret
    /// is stored. The message names the variable, never a value.
    pub fn check_signing_key(&self, settings: &WebhookSettings) -> Result<(), String> {
        resolve_signing_key(settings, self.stored_secret.load_full(), false).map(|_key| ())
    }

    /// Whether the live settings could still resolve a signing secret (or
    /// are deliberately unsigned) once the stored secret is gone. Checked
    /// before `DELETE /admin/webhooks/signing-key` deletes anything.
    ///
    /// # Errors
    /// The live settings name an environment variable this process cannot
    /// see, so the stored secret is the only thing signing deliveries.
    pub fn check_signing_key_without_stored_secret(&self) -> Result<(), String> {
        match self.live_settings() {
            Some(settings) => resolve_signing_key(&settings, None, false).map(|_key| ()),
            None => Ok(()),
        }
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
        resolve_signing_key(settings, self.stored_secret.load_full(), true)
    }
}

/// The resolution behind [`WebhookController::apply`] and its dry runs, over
/// an explicit stored secret. `log` is off for a dry run, so a check that
/// precedes an apply does not print every warning twice.
fn resolve_signing_key(
    settings: &WebhookSettings,
    stored: Option<Arc<SigningKey>>,
    log: bool,
) -> Result<Option<Arc<SigningKey>>, String> {
    // Only variable NAMES appear in any message or log line below.
    if let Some(var) = &settings.signing_key_env {
        return match std::env::var(var) {
            Ok(value) if !value.is_empty() => {
                Ok(Some(Arc::new(SigningKey::new(value.into_bytes()))))
            }
            _ => match stored {
                Some(stored) => {
                    if log {
                        tracing::warn!(
                            "webhooks.signing_key_env names '{var}', which is unset or empty; \
                             using the secret stored through PUT /admin/webhooks/signing-key"
                        );
                    }
                    Ok(Some(stored))
                }
                None => Err(format!(
                    "webhooks.signing_key_env names '{var}', which is unset or empty in the \
                     environment, and no signing secret is stored"
                )),
            },
        };
    }
    if stored.is_some() {
        return Ok(stored);
    }
    if log {
        tracing::warn!(
            "webhooks are enabled with no signing secret: deliveries carry no x-lumen-signature \
             header and the receiver cannot verify authenticity"
        );
    }
    Ok(None)
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

    // ---- Receiver SSRF guard ------------------------------------------------

    /// A resolver answering `addrs` for any host.
    fn resolves_to(
        addrs: &[&str],
    ) -> impl FnOnce(String, u16) -> std::future::Ready<Result<Vec<IpAddr>, String>> {
        let ips: Vec<IpAddr> = addrs
            .iter()
            .map(|a| a.parse().expect("test address"))
            .collect();
        move |_, _| std::future::ready(Ok(ips))
    }

    /// A resolver that must never be consulted.
    fn never_resolves(_: String, _: u16) -> std::future::Ready<Result<Vec<IpAddr>, String>> {
        std::future::ready(Err(
            "the guard resolved a host it should not have".to_owned()
        ))
    }

    #[tokio::test]
    async fn a_hostname_is_refused_when_any_of_its_addresses_is_internal() {
        let url = "https://hooks.example.com/events";
        assert!(
            check_receiver_url(url, &[], resolves_to(&["93.184.215.14"]))
                .await
                .is_ok()
        );
        for addrs in [
            &["169.254.169.254"][..],
            &["10.1.2.3"][..],
            &["93.184.215.14", "127.0.0.1"][..],
            &["::1"][..],
        ] {
            let err = check_receiver_url(url, &[], resolves_to(addrs))
                .await
                .expect_err("an internal address must be refused");
            assert!(err.contains("resolves to an internal address"), "{err}");
            assert!(err.contains(ALLOWED_HOSTS_ENV), "{err}");
        }
    }

    #[tokio::test]
    async fn an_unresolvable_hostname_is_refused() {
        let unresolvable = |_: String, _: u16| {
            std::future::ready(Err::<Vec<IpAddr>, _>("could not be resolved".to_owned()))
        };
        let err = check_receiver_url("https://nowhere.invalid/e", &[], unresolvable)
            .await
            .expect_err("fail closed");
        assert!(err.contains("could not be resolved"), "{err}");
        let err = check_receiver_url("https://nowhere.invalid/e", &[], resolves_to(&[]))
            .await
            .expect_err("no address at all");
        assert!(err.contains("could not be resolved"), "{err}");
    }

    #[tokio::test]
    async fn literals_localhost_and_allowlisted_hosts_are_decided_without_dns() {
        for url in [
            "http://169.254.169.254/latest",
            "http://[fe80::1]/e",
            "http://localhost:8080/e",
            "http://lab.localhost/e",
        ] {
            assert!(
                check_receiver_url(url, &[], never_resolves).await.is_err(),
                "{url}"
            );
        }
        assert!(
            check_receiver_url("https://[2606:4700::1111]/e", &[], never_resolves)
                .await
                .is_ok()
        );
        let allow = parse_receiver_allowlist(" Lab. , .corp.internal,,[::1]");
        assert_eq!(allow, ["lab", ".corp.internal", "::1"]);
        assert!(
            check_receiver_url("http://[::1]:9/e", &allow, never_resolves)
                .await
                .is_ok()
        );
        for url in [
            "http://lab:3000/lumen/events",
            "http://LAB./e",
            "http://hooks.corp.internal/e",
        ] {
            assert!(
                check_receiver_url(url, &allow, never_resolves)
                    .await
                    .is_ok(),
                "{url}"
            );
        }
        assert!(check_receiver_url("ftp://lab/e", &allow, never_resolves)
            .await
            .is_err());
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

    fn stored_row(url: &str) -> Arc<StoredWebhookConfig> {
        Arc::new(StoredWebhookConfig {
            enabled: true,
            settings: WebhookSettings {
                url: url.to_owned(),
                ..WebhookSettings::default()
            },
            updated_at: 0,
        })
    }

    #[tokio::test]
    async fn a_refresh_cannot_overwrite_the_cache_with_a_row_read_before_an_admin_write() {
        // The reload's refresh and an admin write's refresh race. Holding the
        // refresh guard stands in for the admin refresh in progress: the
        // reload's refresh must wait, and so read the row the admin wrote
        // rather than store one it read earlier.
        let store = KeyStore::in_memory().await.expect("store");
        let row = |url: &str| WebhookSettings {
            url: url.to_owned(),
            ..WebhookSettings::default()
        };
        store
            .save_webhook_config(&row("https://old.example/events"))
            .await
            .expect("store the old row");
        let controller = Arc::new(controller());

        let guard = controller.refresh_lock.lock().await;
        let reload_controller = Arc::clone(&controller);
        let reload_store = store.clone();
        let mut reload = tokio::spawn(async move {
            reload_controller
                .refresh_from_store(&reload_store, None)
                .await;
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(200), &mut reload)
                .await
                .is_err(),
            "a refresh must wait for the one in progress"
        );
        // The admin write lands and its own refresh caches it.
        store
            .save_webhook_config(&row("https://new.example/events"))
            .await
            .expect("store the new row");
        controller.stored.store(
            store
                .load_webhook_config()
                .await
                .expect("load")
                .map(Arc::new),
        );
        drop(guard);

        reload.await.expect("reload refresh");
        assert_eq!(
            controller.stored().expect("cached").settings.url,
            "https://new.example/events"
        );
    }

    #[tokio::test]
    async fn a_reload_racing_a_delete_cannot_reapply_the_row_it_cached_before() {
        // A reload refreshed the cache before the DELETE wrote its disabled
        // row, and resolves while the DELETE is still under way. Holding the
        // refresh guard pins the DELETE at its own refresh, so the reload's
        // resolve runs in the middle of it, against the stale enabled row.
        let store = KeyStore::in_memory().await.expect("store");
        let settings = WebhookSettings {
            url: "https://old.example/events".to_owned(),
            ..WebhookSettings::default()
        };
        store
            .save_webhook_config(&settings)
            .await
            .expect("store the enabled row");
        let controller = Arc::new(controller());
        let keys = Arc::new(AuthState::default());
        controller.refresh_from_store(&store, None).await;
        controller
            .resolve_and_apply(None, &keys)
            .expect("the enabled row applies");

        // The DELETE persists its decision, then turns the pipeline off.
        store
            .save_webhook_config_disabled(&settings)
            .await
            .expect("store the disabled row");
        let guard = controller.refresh_lock.lock().await;
        let delete_controller = Arc::clone(&controller);
        let delete_store = store.clone();
        let delete_keys = Arc::clone(&keys);
        let mut delete = tokio::spawn(async move {
            delete_controller
                .disable_from_store(&delete_store, None, &delete_keys)
                .await;
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(200), &mut delete)
                .await
                .is_err(),
            "the DELETE waits for the refresh in progress"
        );
        // The reload resolves from the cache it refreshed before the write.
        controller
            .resolve_and_apply(None, &keys)
            .expect("the reload resolves");
        drop(guard);
        delete.await.expect("delete");

        assert!(
            !controller.stored().expect("cached").enabled,
            "the cache holds the disabled row"
        );
        assert!(
            controller.live_settings().is_none(),
            "the reload re-applied the enabled row the DELETE replaced"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resolve_and_apply_reads_the_cached_row_under_the_apply_lock() {
        // An admin PUT refreshes the cache and applies while a reload is about
        // to resolve. Holding the apply lock stands in for the admin apply in
        // progress; the reload must resolve from the cache as it is once the
        // lock is free, not from a row it read before waiting.
        let controller = Arc::new(controller());
        let keys = Arc::new(AuthState::default());
        controller
            .stored
            .store(Some(stored_row("https://old.example/events")));

        let guard = controller.apply_lock.lock().expect("apply lock");
        let reload_controller = Arc::clone(&controller);
        let reload_keys = Arc::clone(&keys);
        let reload = tokio::task::spawn_blocking(move || {
            reload_controller.resolve_and_apply(None, &reload_keys)
        });
        // Let the blocking thread reach the lock. A thread sleep on purpose:
        // the std guard must not be held across an await.
        std::thread::sleep(Duration::from_millis(200));
        controller
            .stored
            .store(Some(stored_row("https://new.example/events")));
        drop(guard);

        let source = reload.await.expect("join").expect("apply");
        assert_eq!(source, SettingsSource::Database);
        assert_eq!(
            controller.live_settings().expect("enabled").url,
            "https://new.example/events",
            "the reload re-applied a stored row older than the cached one"
        );
    }

    #[tokio::test]
    async fn preflight_and_apply_tell_invalid_settings_from_internal_failures() {
        let keys = AuthState::default();
        let good = settings_with_env(None);

        // The document's own faults.
        let controller = controller();
        let unresolvable = settings_with_env(Some("LUMEN_TEST_WEBHOOK_SECRET_ABSENT"));
        for settings in [&unresolvable, &WebhookSettings::default()] {
            assert!(matches!(
                controller.preflight(settings),
                Err(WebhookApplyError::Invalid(_))
            ));
            assert!(matches!(
                controller.apply(settings, &keys),
                Err(WebhookApplyError::Invalid(_))
            ));
        }

        // A failed Prometheus registration is the gateway's fault.
        let _taken = WebhookMetrics::register(&controller.registry).expect("register");
        assert!(matches!(
            controller.preflight(&good),
            Err(WebhookApplyError::Internal(_))
        ));
        assert!(matches!(
            controller.apply(&good, &keys),
            Err(WebhookApplyError::Internal(_))
        ));

        // So is a poisoned apply lock.
        let controller = Arc::new(self::controller());
        let poisoner = Arc::clone(&controller);
        let _ = std::thread::spawn(move || {
            let _guard = poisoner.apply_lock.lock();
            panic!("poison the apply lock");
        })
        .join();
        assert!(matches!(
            controller.preflight(&good),
            Err(WebhookApplyError::Internal(_))
        ));
        assert!(matches!(
            controller.apply(&good, &keys),
            Err(WebhookApplyError::Internal(_))
        ));
        controller.stored.store(Some(stored_row(&good.url)));
        assert!(matches!(
            controller.resolve_and_apply(None, &keys),
            Err(WebhookApplyError::Internal(_))
        ));
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
