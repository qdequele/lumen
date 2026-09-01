//! Configuration hot reload (M7 §7.3).
//!
//! On `SIGHUP`, a change to the config file, or an admin trigger, the config is
//! re-loaded and **validated**; only if it is valid does the provider routing
//! table swap atomically via the registry's
//! [`ArcSwap`](lumen_providers::Registry). In-flight requests hold a snapshot of
//! the old table (`.load()`), so the swap never disturbs them. An invalid reload
//! is logged, the `lumen_config_reload_failures_total` metric is incremented,
//! and the previous configuration is kept (criterion 3).
//!
//! Scope of a reload (all swapped atomically, off the request path):
//! - the **routing table** (providers, models, aliases, fallbacks);
//! - the **price table** (DEBT-1);
//! - the **resilience policy** (retry/timeouts/fallbacks), circuit-breaker
//!   state preserved;
//! - the safe **auth knobs** ([`AuthKnobs`]: budget-flush cadence and usage-log
//!   retention window), read live by the background tasks on their next tick;
//! - the **virtual-key table**, re-synced from the auth DB so keys created
//!   offline (e.g. `lumen keys create`) become live without a restart;
//!   existing entries keep their in-memory spend (memory stays the source of
//!   truth for accrued spend after boot);
//! - the **whole webhook configuration** (ADR 011 and its amendment). A
//!   reload re-resolves the precedence - a stored row written through
//!   `PUT /admin/webhooks` wins over the `[webhooks]` file block, and a
//!   stored row marked disabled means off whatever the file says - then
//!   applies the winner. Retuning `url`, `events`, `thresholds` or the retry
//!   knobs keeps the bounded queue and its sender task, so a retarget never
//!   drops what is already queued; changing `channel_capacity` replaces both,
//!   with the previous sender draining the events it had already accepted
//!   before it exits. Removing the `[webhooks]` block stops detection only
//!   when no enabled stored row exists - precedence means an enabled row keeps
//!   delivering whatever the file says, until `DELETE /admin/webhooks` marks
//!   it disabled.
//!
//! Read once at boot and therefore **restart-only** (documented in
//! `docs/backlog.md`): the server bind address (rebinding a live listener is
//! high-risk and out of scope), `auth.enabled`, `auth.db_path`, the bounded
//! usage-log channel knobs (`usage_channel_capacity`, `usage_batch_max`,
//! `usage_flush_ms`) whose capacity is structurally fixed at channel creation.
//!
//! Webhooks have no restart-only field left: with auth enabled the
//! [`WebhookController`] exists whether or not a `[webhooks]` block does, and
//! its `apply` creates the queue and sender from nothing (or rebuilds them for
//! a new `channel_capacity`), so a block added to a running process takes
//! effect on the next reload. The one thing no reload can do is observe a
//! *newly set* environment variable, because a running process cannot: pointing
//! `signing_key_env` at a variable that was unset at startup needs a restart,
//! or `PUT /admin/webhooks/signing-key`, which needs no variable at all. With
//! auth *disabled* there is no controller and no `/admin` surface, so enabling
//! webhooks then does require a restart.
//!
//! Provider API keys are re-resolved from the environment on every reload (env
//! stays the primary source). For providers whose env var is unset, the key is
//! re-read from the encrypted DB store on every reload via [`ProviderKeySource`],
//! so rotating a DB-stored key (`PUT /admin/provider-keys`) takes effect on the
//! next reload with no restart. The DB read runs only in the reload task; a DB
//! error keeps the previous snapshot so a reload never strips a working key.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use lumen_auth::crypto::MasterKey;
use lumen_auth::store::KeyStore;
use lumen_providers::{Registry, RegistryError};
use lumen_telemetry::ReloadMetrics;
use tokio::sync::Notify;

use crate::config::{Config, ConfigError};
use crate::config_source::ConfigContext;
use crate::pricing::CostTable;
use crate::resilience::ResilienceRuntime;
use crate::webhooks::WebhookController;

/// Why a reload was rejected. The previous config is always kept on error.
#[derive(Debug, thiserror::Error)]
pub enum ReloadError {
    /// The new config file was missing, unparseable or failed validation.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// The config validated but the registry could not be rebuilt from it.
    #[error(transparent)]
    Registry(#[from] RegistryError),
}

/// Auth operational knobs that are safe to swap at runtime: the budget-flush
/// cadence and the usage-log retention window. Both are read *live* by their
/// background tasks on each tick, so a reload takes effect on the next tick with
/// no restart. The bounded usage-log channel (`usage_channel_capacity`,
/// `usage_batch_max`, `usage_flush_ms`), the database path and the `enabled`
/// switch are structural and remain restart-only (see the module docs).
#[derive(Debug)]
pub struct AuthKnobs {
    flush_interval_ms: AtomicU64,
    retention_days: AtomicU32,
}

impl AuthKnobs {
    /// A knob cell seeded from explicit values.
    #[must_use]
    pub fn new(flush_interval_ms: u64, retention_days: u32) -> Self {
        Self {
            flush_interval_ms: AtomicU64::new(flush_interval_ms),
            retention_days: AtomicU32::new(retention_days),
        }
    }

    /// A knob cell seeded from the loaded config's `[auth]` section.
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        Self::new(config.auth.flush_interval_ms, config.auth.retention_days)
    }

    /// Current budget-flush interval, in milliseconds.
    #[must_use]
    pub fn flush_interval_ms(&self) -> u64 {
        self.flush_interval_ms.load(Ordering::Relaxed)
    }

    /// Current usage-log retention window, in days.
    #[must_use]
    pub fn retention_days(&self) -> u32 {
        self.retention_days.load(Ordering::Relaxed)
    }

    /// Overwrite both knobs from a freshly loaded config (called on reload).
    fn store_from_config(&self, config: &Config) {
        self.flush_interval_ms
            .store(config.auth.flush_interval_ms, Ordering::Relaxed);
        self.retention_days
            .store(config.auth.retention_days, Ordering::Relaxed);
    }
}

/// The encrypted-DB provider-key source consulted on every reload so a rotated
/// key (`PUT /admin/provider-keys`) is picked up without a restart. `Some` in
/// [`ReloadTargets`] only when auth is enabled. Kept strictly off the request
/// path: the decrypting read happens only in the reload task.
pub struct ProviderKeySource {
    store: KeyStore,
    master: MasterKey,
    provider_names: Vec<String>,
}

impl ProviderKeySource {
    /// Build a source over `store`, decrypting with `master`, for exactly the
    /// configured `provider_names` (bounded by config, never client input).
    #[must_use]
    pub fn new(store: KeyStore, master: MasterKey, provider_names: Vec<String>) -> Self {
        Self {
            store,
            master,
            provider_names,
        }
    }
}

/// The process-wide handles a reload swaps: the routing table, the price table,
/// the resilience policy (the circuit breakers inside `resilience` are
/// deliberately *not* swapped - their live state survives the reload) and the
/// safe auth knobs. Bundled so the reload signature stays small and future
/// config surfaces can join.
pub struct ReloadTargets {
    /// The provider routing table (its own `ArcSwap` inside).
    pub registry: Arc<Registry>,
    /// The price table cell (DEBT-1).
    pub pricing: Arc<ArcSwap<CostTable>>,
    /// The resilience runtime; only its policy cell is swapped.
    pub resilience: Arc<ResilienceRuntime>,
    /// Reload success/failure counters.
    pub metrics: ReloadMetrics,
    /// Live DB provider-key snapshot, refreshed from [`key_source`](Self::key_source)
    /// on every reload and merged into any env-keyless provider so a reload
    /// never strips a stored key (env still wins). Behind an `ArcSwap` so the
    /// async reload task can refresh it before the synchronous registry rebuild
    /// reads it.
    pub key_backfill: Arc<ArcSwap<HashMap<String, String>>>,
    /// DB key source re-read on each reload (rotation without restart); `Some`
    /// only when auth is enabled.
    pub key_source: Option<Arc<ProviderKeySource>>,
    /// Live auth knobs swapped from the reloaded config; `Some` only when auth
    /// is enabled.
    pub auth_knobs: Option<Arc<AuthKnobs>>,
    /// The outbound-webhook control surface (ADR 011); `Some` whenever auth
    /// is enabled, whether or not webhooks are currently on. A reload
    /// re-resolves the stored row against the file block through it.
    pub webhooks: Option<Arc<WebhookController>>,
    /// The live auth runtime (in-memory key table + store); `Some` only when
    /// auth is enabled. On each reload the virtual-key table is re-read from
    /// the DB so keys created offline (e.g. `lumen keys create`) become live
    /// without a restart; existing entries only have their limits re-applied
    /// and keep their in-memory spend.
    pub auth_runtime: Option<Arc<crate::auth::AuthRuntime>>,
}

/// Atomically swap the routing table, price table, resilience policy and
/// auth knobs from an already loaded-and-validated `config`. Increments the
/// success/failure counters. On any error every target is left exactly as it
/// was (the fallible registry rebuild runs first, before any swap).
///
/// `config` is expected to already be the result of a successful
/// [`ConfigContext::load_config`] - this function does no parsing or file
/// I/O of its own, only the in-memory rebuild and swap, which is why
/// [`reload_once`] can run it without a further `spawn_blocking` hop.
///
/// The DB provider-key snapshot in `targets.key_backfill` is read as-is here;
/// [`reload_once`] refreshes it from the DB (async) before calling this.
///
/// # Errors
/// [`ReloadError::Registry`] if the registry cannot be rebuilt from `config`;
/// the running config is unaffected.
pub fn apply_reload(config: &Config, targets: &ReloadTargets) -> Result<(), ReloadError> {
    // `provider_specs` resolves keys from the environment; re-apply the current
    // DB-key snapshot for any provider still keyless, mirroring boot back-fill
    // so a reload never strips a DB-stored key (env keeps precedence). The
    // snapshot was refreshed from the DB by `reload_once` just before this.
    let mut specs = config.provider_specs();
    let backfill = targets.key_backfill.load_full();
    merge_key_backfill(&mut specs, &backfill);
    // The fallible step goes FIRST: the registry rebuild is the last line of
    // defence (a keyless provider missing a base_url surfaces here). On failure
    // nothing has been swapped yet, so every target keeps its old value.
    if let Err(error) = targets.registry.reload(specs) {
        targets.metrics.inc_failure();
        tracing::warn!(%error, "config reload rejected by registry; keeping the running config");
        return Err(error.into());
    }
    // Registry swapped; the remaining swaps are infallible.
    targets
        .pricing
        .store(Arc::new(CostTable::from_config(config)));
    targets.resilience.reload_policy(config);
    if let Some(knobs) = &targets.auth_knobs {
        knobs.store_from_config(config);
    }
    apply_webhook_reload(config, targets);
    targets.metrics.inc_success();
    tracing::info!(
        model_count = config.loaded_models().len(),
        provider_count = config.providers.len(),
        "configuration reloaded; routing table, pricing, resilience policy and auth knobs swapped"
    );
    Ok(())
}

/// Retune outbound webhooks from a reloaded config (ADR 011 §4).
///
/// The queue and the sender task survive the reload; only the delivery policy
/// and the signalling policy (enabled events, thresholds) are swapped. Two
/// cases are reported rather than applied, because neither can be honoured
/// without a restart: a `[webhooks]` block added to a process that booted
/// without one (there is no queue to attach to), and the structural knobs
/// `apply_reload` warns about.
fn apply_webhook_reload(config: &Config, targets: &ReloadTargets) {
    // Both are `Some` exactly when auth is enabled, and `Config::validate`
    // refuses `[webhooks]` without it.
    let (Some(webhooks), Some(auth_runtime)) = (&targets.webhooks, &targets.auth_runtime) else {
        if config.webhooks.is_some() {
            tracing::warn!(
                "config declares [webhooks] but auth is disabled in this process; outbound \
                 budget events need auth.enabled = true and a restart"
            );
        }
        return;
    };
    // The stored row was refreshed by `reload_once` just before this.
    match webhooks.resolve_and_apply(config.webhooks.as_ref(), &auth_runtime.keys) {
        Ok(source) => tracing::debug!(?source, "webhook configuration resolved"),
        // A bad webhook block must not reject the whole reload: routing,
        // pricing and resilience have already swapped, and refusing them over
        // a signalling convenience would be the wrong trade. The previous
        // pipeline keeps running and the operator gets a warning.
        Err(error) => tracing::warn!(
            %error,
            "webhook configuration rejected; keeping the previous webhook settings"
        ),
    }
}

/// Re-read every configured provider's key from the encrypted DB store. A
/// provider with no stored key is simply absent from the map (env stays the
/// primary source; [`merge_key_backfill`] only fills env-keyless specs). Runs in
/// the reload task, never on the request path.
///
/// # Errors
/// Propagates the first DB/decryption error; the caller keeps the previous
/// snapshot on failure so a sick DB never strips a working key.
pub async fn refresh_provider_keys(
    source: &ProviderKeySource,
) -> Result<HashMap<String, String>, lumen_auth::AuthError> {
    let mut fresh = HashMap::new();
    for name in &source.provider_names {
        if let Some(key) = source.store.load_provider_key(name, &source.master).await? {
            fresh.insert(name.clone(), key);
        }
    }
    Ok(fresh)
}

/// Re-apply DB-boot-time provider keys to any spec still keyless after env
/// resolution. Env keys win (a spec with a resolved env key is left untouched).
#[allow(clippy::implicit_hasher)]
fn merge_key_backfill(
    specs: &mut [lumen_providers::ProviderSpec],
    key_backfill: &HashMap<String, String>,
) {
    for spec in specs {
        if spec.api_key.is_none() {
            if let Some(key) = key_backfill.get(&spec.name) {
                spec.api_key = Some(key.clone());
            }
        }
    }
}

/// Debounce window: coalesce a burst of file-system events (editors often write
/// a config in several syscalls) into one reload.
const DEBOUNCE: Duration = Duration::from_millis(250);

/// The directory `spawn_config_reloader` should watch for `path`: the parent
/// directory when `path` has one, or the current working directory when it
/// does not (e.g. `lumen --config lumen.toml`, the form used in the
/// quickstart, has an empty parent). Mirrors `config_source::sync_parent_dir`'s
/// identical fallback for the identical empty-parent case.
///
/// Watching `path` itself instead of its directory (the bug this function
/// fixes) works right up until something replaces the file via rename -
/// which `PUT /admin/config` and any GitOps sync both do - at which point
/// the watch dies silently with no error anywhere, because the rename
/// unlinks the inode the watch was armed on.
fn watch_target(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map_or_else(|| Path::new("."), |p| p)
}

/// Whether a file-system event from the config directory watch should schedule
/// a reload. Two independent filters, both of which must pass.
///
/// **Kind.** The watch is armed on the config's parent directory, and
/// `notify`'s inotify backend arms it with a 0xfee mask that includes
/// `IN_OPEN` (0x20). A reload *opens* the config file to re-read it, so
/// treating an open as a change made every reload schedule the next one: a
/// self-sustaining loop, one reload per debounce window (~4/second) until the
/// process was restarted, which any process merely *reading* the file could
/// start - including LUMEN's own `lumen --check-config` (the documented
/// pre-reload safety check) and `lumen keys list`. Non-mutating accesses are
/// therefore ignored. The single exception is
/// `Access(Close(AccessMode::Write))`, which is how the inotify backend
/// reports `IN_CLOSE_WRITE`: an `Access` event that nonetheless marks a
/// *completed write*. Filtering on the event kind rather than on an explicit
/// inotify mask keeps this backend-agnostic (FSEvents, kqueue and the polling
/// fallback report no opens at all, and none of them is harmed by the check).
///
/// **Path.** The event must name the config file itself, so a neighbour in the
/// same directory (the auth SQLite DB and its WAL, `lumen.env`, a staged temp
/// file mid-rename) is not mistaken for a config change. Matching on the file
/// name avoids `canonicalize` races while the file is briefly absent mid-rename.
fn event_should_reload(event: &notify::Event, config_name: Option<&std::ffi::OsStr>) -> bool {
    use notify::event::{AccessKind, AccessMode};
    let may_have_changed_the_bytes = match event.kind {
        // IN_CLOSE_WRITE: a write that just finished.
        notify::EventKind::Access(AccessKind::Close(AccessMode::Write)) => true,
        // IN_OPEN, reads, read-only closes: nothing changed. Reacting to
        // these is the feedback loop described above.
        notify::EventKind::Access(_) => false,
        // Create / Modify / Remove, plus the `Any`/`Other` catch-alls a
        // backend uses for masks it cannot map precisely: assume a change and
        // let the reload itself decide (an invalid or unchanged config is
        // cheap and already handled).
        _ => true,
    };
    may_have_changed_the_bytes && event.paths.iter().any(|p| p.file_name() == config_name)
}

/// Spawn the background reloader: reload on `SIGHUP`, on a change to `ctx`'s
/// watched path (file mode only), and when `trigger` is notified (the admin
/// API pings it after storing a provider key, so a rotation applies without a
/// restart). The returned task runs until the process exits; the file
/// watcher, when armed, is kept alive inside it.
///
/// The `notify` file watcher is armed only when `ctx.source.watch_path()` is
/// `Some`: a DB-backed source has no on-disk mirror to watch, so a change can
/// only ever arrive through `ConfigSource::persist` itself, which the admin
/// trigger already covers. SIGHUP and the admin trigger are armed in both
/// modes.
///
/// # Errors
/// Returns the `notify` error if the file watcher cannot be created or armed
/// (file mode only); the caller should log it and continue (hot reload via
/// SIGHUP and the admin trigger still work if the watcher fails).
pub fn spawn_config_reloader(
    ctx: Arc<ConfigContext>,
    targets: ReloadTargets,
    trigger: Arc<Notify>,
) -> Result<tokio::task::JoinHandle<()>, notify::Error> {
    use notify::{RecursiveMode, Watcher};

    // A channel that a file-mode watcher's callback pushes onto. Created
    // unconditionally, but only ever pushed to when a watcher exists below:
    // in DB mode `_tx` (kept alive in the spawned task, see below) is the
    // channel's only sender, so `rx.recv()` simply never resolves - a
    // permanently idle `select!` branch, not a disconnect.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();

    // Watch the parent directory (editors, GitOps syncs and `PUT
    // /admin/config` all replace the file via rename rather than an
    // in-place write, which a watch on the file itself would miss - a
    // rename unlinks the inode a file-level watch is armed on, most visibly
    // with the inotify backend, silently ending the watch with no error
    // anywhere). A directory watch is necessarily broader than the config
    // file and broader than "the bytes changed", so `event_should_reload`
    // narrows it back down on both axes: by kind (an open must never
    // schedule a reload, or the reload's own read of the file loops
    // forever) and by path (a neighbour file must not trigger a reload).
    let watcher = match ctx.source.watch_path() {
        Some(path) => {
            let path = path.to_path_buf();
            let config_name = path.file_name().map(std::ffi::OsStr::to_owned);
            let tx = tx.clone();
            let mut watcher =
                notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                    if let Ok(event) = res {
                        if event_should_reload(&event, config_name.as_deref()) {
                            // Non-blocking; a full/closed channel just drops
                            // the tick (the next event, or the debounce
                            // drain, still triggers a reload).
                            let _ = tx.send(());
                        }
                    }
                })?;
            watcher.watch(watch_target(&path), RecursiveMode::NonRecursive)?;
            Some(watcher)
        }
        None => None,
    };

    let targets = Arc::new(targets);
    let handle = tokio::spawn(async move {
        // Keep the watcher (if armed) and this scope's own sender alive for
        // the lifetime of the task - see the comment on `tx` above.
        let _watcher = watcher;
        let _tx = tx;
        let mut sighup = hangup_signal();
        loop {
            tokio::select! {
                () = wait_for_hangup(&mut sighup) => {
                    tracing::info!("SIGHUP received; reloading config");
                    reload_once(&ctx, &targets).await;
                }
                () = trigger.notified() => {
                    tracing::info!("admin reload trigger fired; reloading config");
                    reload_once(&ctx, &targets).await;
                }
                event = rx.recv() => {
                    if event.is_none() {
                        break; // sender dropped (never, in practice: `_tx` above)
                    }
                    // Coalesce the rest of the burst before reloading.
                    tokio::time::sleep(DEBOUNCE).await;
                    while rx.try_recv().is_ok() {}
                    reload_once(&ctx, &targets).await;
                }
            }
        }
    });
    Ok(handle)
}

/// Run one reload: refresh the DB provider-key snapshot (async, in this task,
/// off the request path), load and validate the current document via
/// `ctx.load_config()`, then apply it on a blocking thread (the registry
/// rebuild's HTTP-client construction, kept off the runtime worker - CLAUDE.md
/// rule 2 in spirit; the actual figment/file work already ran inside
/// `load_config`). A DB refresh error keeps the previous snapshot; a load or
/// validation error keeps the running config (ADR 008's sick-source rule,
/// extended to the whole document). Public so the boot path and the tests
/// share exactly one reload entry point.
pub async fn reload_once(ctx: &Arc<ConfigContext>, targets: &Arc<ReloadTargets>) {
    // Rotation without restart: re-read provider keys from the encrypted DB.
    // Keep the previous snapshot on any error so a sick DB never strips a key.
    if let Some(source) = &targets.key_source {
        match refresh_provider_keys(source).await {
            Ok(fresh) => targets.key_backfill.store(Arc::new(fresh)),
            Err(error) => tracing::warn!(
                %error,
                "provider-key refresh failed; keeping the previous DB-key snapshot"
            ),
        }
    }
    // Virtual keys and budget groups created offline (e.g. `lumen keys
    // create` straight against the DB) become live here: re-read the tables
    // and upsert into the in-memory state. Groups refresh FIRST so the key
    // pass can resolve its group pointers (ADR 009). New entries are
    // inserted; existing ones only have their limits re-applied and keep
    // their in-memory spend. A DB error keeps the current tables (a sick DB
    // never locks anyone out).
    if let Some(runtime) = &targets.auth_runtime {
        match runtime.store.load_groups().await {
            Ok(groups) => {
                for record in &groups {
                    runtime.keys.upsert_group(record);
                }
            }
            Err(error) => tracing::warn!(
                %error,
                "budget-group refresh failed; keeping the current in-memory group table"
            ),
        }
        match runtime.store.load_auth_entries().await {
            Ok(entries) => {
                for (hash, record) in entries {
                    runtime.keys.upsert(hash, &record);
                }
            }
            Err(error) => tracing::warn!(
                %error,
                "virtual-key refresh failed; keeping the current in-memory key table"
            ),
        }
    }
    // The webhook config and its sealed signing secret live in the same DB;
    // refresh the cache the synchronous resolve below reads (ADR 011
    // amendment §2). Errors keep the previous cache, logged inside.
    if let (Some(webhooks), Some(runtime)) = (&targets.webhooks, &targets.auth_runtime) {
        webhooks
            .refresh_from_store(&runtime.store, runtime.master.as_ref())
            .await;
    }
    // Load and fully validate the current document. Any failure here - the
    // source itself unreadable, or the document invalid - keeps the running
    // config exactly as it was: the ADR 008 rule that a sick source never
    // strips a working gateway, extended from "the DB is unreachable" to
    // "the whole document failed to load".
    let config = match ctx.load_config().await {
        Ok(config) => config,
        Err(error) => {
            targets.metrics.inc_failure();
            tracing::warn!(%error, "config reload rejected; keeping the running config");
            return;
        }
    };
    let targets = Arc::clone(targets);
    let joined = tokio::task::spawn_blocking(move || {
        let _ = apply_reload(&config, &targets);
    })
    .await;
    if let Err(error) = joined {
        tracing::warn!(%error, "config reload task panicked");
    }
}

#[cfg(unix)]
type Hangup = tokio::signal::unix::Signal;

/// A SIGHUP stream, or `None` if one could not be installed (never panics).
#[cfg(unix)]
fn hangup_signal() -> Option<Hangup> {
    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()).ok()
}

#[cfg(unix)]
async fn wait_for_hangup(sighup: &mut Option<Hangup>) {
    match sighup {
        Some(stream) => {
            stream.recv().await;
        }
        // No handler: this branch simply never fires.
        None => std::future::pending::<()>().await,
    }
}

#[cfg(not(unix))]
type Hangup = ();

#[cfg(not(unix))]
fn hangup_signal() -> Option<Hangup> {
    None
}

#[cfg(not(unix))]
async fn wait_for_hangup(_sighup: &mut Option<Hangup>) {
    std::future::pending::<()>().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_providers::http;
    use lumen_telemetry::Metrics;
    use std::io::Write;
    use std::path::PathBuf;

    fn write_config(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("config.toml");
        let mut file = std::fs::File::create(&path).expect("write config");
        file.write_all(body.as_bytes()).expect("write config body");
        path
    }

    const ONE_MODEL: &str = r#"
        [[providers]]
        name = "openai"
        kind = "openai"
        [[providers.models]]
        id = "gpt"
        capabilities = ["chat"]
    "#;

    const TWO_MODELS: &str = r#"
        [[providers]]
        name = "openai"
        kind = "openai"
        [[providers.models]]
        id = "gpt"
        capabilities = ["chat"]
        [[providers.models]]
        id = "embed"
        capabilities = ["embed"]
    "#;

    fn registry_from(path: &Path) -> Arc<Registry> {
        let config = Config::load(path).expect("initial config valid");
        Arc::new(
            Registry::build(
                config.provider_specs(),
                http::build_client(),
                std::time::Duration::from_secs(300),
            )
            .expect("registry"),
        )
    }

    /// Load and validate `path` into a `Config`, the way a caller of
    /// `apply_reload` is expected to have already done via
    /// `ConfigContext::load_config` before calling it.
    fn load(path: &Path) -> Config {
        Config::load(path).expect("config loads")
    }

    /// A file-mode `ConfigContext` over `path`, for `reload_once` and
    /// `spawn_config_reloader` tests.
    fn ctx(path: &Path) -> Arc<ConfigContext> {
        Arc::new(ConfigContext::file(path.to_path_buf()))
    }

    /// Reload targets sharing `registry`/`metrics`, with default pricing and
    /// resilience, no key backfill and no auth knobs.
    fn targets(registry: Arc<Registry>, metrics: ReloadMetrics) -> ReloadTargets {
        ReloadTargets {
            registry,
            pricing: Arc::new(ArcSwap::from_pointee(CostTable::default())),
            resilience: Arc::new(ResilienceRuntime::defaults()),
            metrics,
            key_backfill: Arc::new(ArcSwap::from_pointee(HashMap::new())),
            key_source: None,
            auth_knobs: None,
            webhooks: None,
            auth_runtime: None,
        }
    }

    #[test]
    fn valid_reload_swaps_the_routing_table() {
        let dir = tempdir();
        let path = write_config(&dir, ONE_MODEL);
        let registry = registry_from(&path);
        assert!(registry.chat_route("gpt").is_some());
        assert!(registry.embedding_route("embed").is_none());

        let metrics = ReloadMetrics::register(&Metrics::new()).unwrap();
        let t = targets(Arc::clone(&registry), metrics);
        write_config(&dir, TWO_MODELS);
        apply_reload(&load(&path), &t).expect("valid reload");

        // The new model is now routable - the swap took effect.
        assert!(registry.embedding_route("embed").is_some());
        assert!(registry.knows_model("gpt"));
    }

    #[test]
    #[allow(clippy::float_cmp)] // prices come straight from config: exact
    fn valid_reload_swaps_pricing_and_resilience_but_keeps_breaker_state() {
        use lumen_router::circuit::CircuitState;
        let dir = tempdir();
        // Start with no price and no fallback.
        let path = write_config(&dir, ONE_MODEL);
        let registry = registry_from(&path);
        let t = targets(
            Arc::clone(&registry),
            ReloadMetrics::register(&Metrics::new()).unwrap(),
        );

        // Baseline: model unpriced, no fallback chain.
        assert_eq!(t.pricing.load().token_cost("gpt", 1_000_000, 0), 0.0);
        assert_eq!(t.resilience.chain_ids("gpt"), vec!["gpt"]);

        // Trip the breaker for (openai, gpt) so we can prove it survives reload.
        let breaker = t.resilience.breakers.get("openai", "gpt");
        let now = tokio::time::Instant::now();
        // Default threshold is 5 consecutive failures.
        for _ in 0..5 {
            breaker.on_failure(now);
        }
        assert_eq!(breaker.state(), CircuitState::Open);

        // Reload with a price + a fallback for gpt.
        write_config(
            &dir,
            r#"
            [[providers]]
            name = "openai"
            kind = "openai"
            [[providers.models]]
            id = "gpt"
            capabilities = ["chat"]
            cost_per_1m_input = 2.5
            fallbacks = ["backup"]
            [[providers.models]]
            id = "backup"
            capabilities = ["chat"]
            "#,
        );
        apply_reload(&load(&path), &t).expect("valid reload");

        // Pricing + resilience policy swapped...
        assert_eq!(t.pricing.load().token_cost("gpt", 1_000_000, 0), 2.5);
        assert_eq!(t.resilience.chain_ids("gpt"), vec!["gpt", "backup"]);
        // ...but the breaker's live state was preserved across the swap.
        assert_eq!(
            t.resilience.breakers.get("openai", "gpt").state(),
            CircuitState::Open,
            "reload must not reset circuit-breaker state"
        );
    }

    #[test]
    fn valid_reload_swaps_the_auth_knobs() {
        let dir = tempdir();
        // Boot config: auth on with the default flush cadence and retention.
        let boot = r#"
            [auth]
            enabled = true
            flush_interval_ms = 10000
            retention_days = 30
            [[providers]]
            name = "openai"
            kind = "openai"
            [[providers.models]]
            id = "gpt"
            capabilities = ["chat"]
        "#;
        let path = write_config(&dir, boot);
        let registry = registry_from(&path);
        let knobs = Arc::new(AuthKnobs::new(10_000, 30));
        let mut t = targets(
            Arc::clone(&registry),
            ReloadMetrics::register(&Metrics::new()).unwrap(),
        );
        t.auth_knobs = Some(Arc::clone(&knobs));

        // Baseline: the boot values.
        assert_eq!(knobs.flush_interval_ms(), 10_000);
        assert_eq!(knobs.retention_days(), 30);

        // Reload with new operator-tuned knobs.
        write_config(
            &dir,
            r#"
            [auth]
            enabled = true
            flush_interval_ms = 2500
            retention_days = 7
            [[providers]]
            name = "openai"
            kind = "openai"
            [[providers.models]]
            id = "gpt"
            capabilities = ["chat"]
            "#,
        );
        apply_reload(&load(&path), &t).expect("valid reload");

        // The live knobs the background tasks read now reflect the new config,
        // with no restart. The very cell handed to those tasks was swapped.
        assert_eq!(knobs.flush_interval_ms(), 2_500);
        assert_eq!(knobs.retention_days(), 7);
    }

    #[tokio::test]
    async fn reload_re_reads_a_rotated_db_provider_key() {
        use lumen_auth::store::KeyStore;

        let dir = tempdir();
        // A cohere provider with NO env key: its key comes from the DB store.
        let body = r#"
            [[providers]]
            name = "cohere"
            kind = "cohere"
            [[providers.models]]
            id = "rr"
            capabilities = ["rerank"]
        "#;
        let path = write_config(&dir, body);

        let store = KeyStore::in_memory().await.expect("store");
        // Two master handles from the same 64-hex value decrypt identically;
        // one lives in the key source, the other drives the admin store calls.
        let admin_master = MasterKey::from_env_value(&"a".repeat(64)).expect("master");
        let source_master = MasterKey::from_env_value(&"a".repeat(64)).expect("master");
        store
            .store_provider_key("cohere", "old-key", &admin_master)
            .await
            .expect("store old key");

        // Boot the registry with the boot snapshot (old key).
        let source = Arc::new(ProviderKeySource::new(
            store.clone(),
            source_master,
            vec!["cohere".to_owned()],
        ));
        let boot_backfill = refresh_provider_keys(&source).await.expect("boot backfill");
        assert_eq!(
            boot_backfill.get("cohere").map(String::as_str),
            Some("old-key")
        );

        let registry = registry_from(&path);
        let t = Arc::new(ReloadTargets {
            registry: Arc::clone(&registry),
            pricing: Arc::new(ArcSwap::from_pointee(CostTable::default())),
            resilience: Arc::new(ResilienceRuntime::defaults()),
            metrics: ReloadMetrics::register(&Metrics::new()).unwrap(),
            key_backfill: Arc::new(ArcSwap::from_pointee(boot_backfill)),
            key_source: Some(source),
            auth_knobs: None,
            webhooks: None,
            auth_runtime: None,
        });

        // Rotate the DB key, then run one reload through the real entry point.
        store
            .store_provider_key("cohere", "new-key", &admin_master)
            .await
            .expect("rotate key");
        reload_once(&ctx(&path), &t).await;

        // The reloaded backfill now carries the rotated key, so the rebuilt
        // registry provider will authenticate with it (env stays unset here).
        assert_eq!(
            t.key_backfill.load().get("cohere").map(String::as_str),
            Some("new-key"),
            "reload must re-read the rotated DB key without a restart"
        );
        assert!(
            registry.rerank_route("rr").is_some(),
            "registry still routes"
        );
    }

    #[tokio::test]
    async fn reload_picks_up_a_virtual_key_created_offline_in_the_db() {
        use crate::auth::{now_unix, AuthRuntime};
        use lumen_auth::key::hash_key;
        use lumen_auth::state::AuthState;
        use lumen_auth::store::{KeyStore, NewKey};

        let dir = tempdir();
        let path = write_config(&dir, ONE_MODEL);
        let registry = registry_from(&path);

        // A live auth runtime whose in-memory table was loaded at "boot",
        // before the offline key existed.
        let store = KeyStore::in_memory().await.expect("store");
        let runtime = Arc::new(AuthRuntime {
            keys: AuthState::load(Vec::new(), Vec::new()),
            store: store.clone(),
            admin_token_hash: hash_key("admin"),
            master: None,
        });

        // Simulate `lumen keys create`: a key written straight to the DB,
        // which the live in-memory table has never seen.
        let (plaintext, _record) = store
            .create_key(NewKey {
                name: "cli-created".to_owned(),
                ..NewKey::default()
            })
            .await
            .expect("create key offline");
        assert!(
            runtime
                .keys
                .authenticate(plaintext.reveal(), now_unix())
                .is_none(),
            "the offline-created key must not be live before the reload"
        );

        let mut t = targets(
            Arc::clone(&registry),
            ReloadMetrics::register(&Metrics::new()).unwrap(),
        );
        t.auth_runtime = Some(Arc::clone(&runtime));
        let t = Arc::new(t);
        reload_once(&ctx(&path), &t).await;

        assert!(
            runtime
                .keys
                .authenticate(plaintext.reveal(), now_unix())
                .is_some(),
            "a config reload must make the offline-created key live, no restart"
        );
    }

    // A `Config`-level failure (parse or validation) now surfaces through
    // `ConfigContext::load_config` rather than `apply_reload` (which takes an
    // already-parsed `&Config`), so these two "keep-previous" cases go
    // through `reload_once` - the real entry point that owns the load step -
    // instead of calling `apply_reload` directly.

    #[tokio::test]
    async fn invalid_reload_keeps_the_old_table_and_counts_the_failure() {
        let dir = tempdir();
        let path = write_config(&dir, TWO_MODELS);
        let registry = registry_from(&path);
        assert!(registry.embedding_route("embed").is_some());

        let metrics = Metrics::new();
        let reload = ReloadMetrics::register(&metrics).unwrap();
        // Overwrite with a config that fails validation (duplicate model id).
        write_config(
            &dir,
            r#"
            [[providers]]
            name = "a"
            kind = "openai"
            [[providers.models]]
            id = "dup"
            capabilities = ["chat"]
            [[providers]]
            name = "b"
            kind = "openai"
            [[providers.models]]
            id = "dup"
            capabilities = ["chat"]
            "#,
        );
        let t = Arc::new(targets(Arc::clone(&registry), reload));
        reload_once(&ctx(&path), &t).await;

        // Old routing table intact: the pre-reload models still resolve.
        assert!(registry.embedding_route("embed").is_some());
        assert!(registry.chat_route("gpt").is_some());
        // Failure counted, no success.
        let out = metrics.encode_text();
        assert!(out.contains("lumen_config_reload_failures_total 1"));
        assert!(out.contains("lumen_config_reloads_total 0"));
    }

    #[test]
    fn reload_to_an_embed_model_on_an_embeddingless_kind_is_rejected() {
        // A groq embed model (no base_url override) is a guaranteed upstream
        // 404, caught by the registry rebuild (issue #74): the reload must
        // fail with the registry error and keep the old routing table. This
        // is a REGISTRY failure, not a `Config`-load failure, so `Config::load`
        // itself succeeds and `apply_reload` can still be called directly.
        let dir = tempdir();
        let path = write_config(&dir, TWO_MODELS);
        let registry = registry_from(&path);
        assert!(registry.embedding_route("embed").is_some());

        let metrics = Metrics::new();
        let reload = ReloadMetrics::register(&metrics).unwrap();
        write_config(
            &dir,
            r#"
            [[providers]]
            name = "groq"
            kind = "groq"
            [[providers.models]]
            id = "groq-embed"
            capabilities = ["embed"]
            "#,
        );
        let t = targets(Arc::clone(&registry), reload);
        let err = apply_reload(&load(&path), &t).unwrap_err();
        assert!(
            matches!(
                err,
                ReloadError::Registry(RegistryError::NoUpstreamEmbeddings { .. })
            ),
            "expected NoUpstreamEmbeddings, got: {err:?}"
        );

        // Old routing table intact: the pre-reload models still resolve, and
        // the rejected model never appeared.
        assert!(registry.embedding_route("embed").is_some());
        assert!(registry.chat_route("gpt").is_some());
        assert!(registry.embedding_route("groq-embed").is_none());
    }

    #[tokio::test]
    async fn reload_of_a_deleted_file_is_rejected_and_keeps_the_table() {
        let dir = tempdir();
        let path = write_config(&dir, ONE_MODEL);
        let registry = registry_from(&path);
        let metrics = Metrics::new();
        let reload = ReloadMetrics::register(&metrics).unwrap();

        std::fs::remove_file(&path).expect("remove config");
        let t = Arc::new(targets(Arc::clone(&registry), reload));
        reload_once(&ctx(&path), &t).await;

        assert!(registry.chat_route("gpt").is_some(), "old table kept");
        assert!(metrics
            .encode_text()
            .contains("lumen_config_reload_failures_total 1"));
    }

    /// ADR 012 DB-mode reload, mirroring the file-mode tests above but over a
    /// `ConfigContext::db`: a document persisted straight to the
    /// `config_versions` table (as `PUT /admin/config` will in the granular
    /// DB-mode rework) becomes live on the very next `reload_once`, with no
    /// restart. Then the ADR 008 sick-source rule, extended to the whole
    /// document (module docs): once the table itself is gone, `reload_once`
    /// must keep the last-known-good registry rather than strip it.
    #[tokio::test]
    async fn db_mode_reload_resolves_a_persisted_doc_then_keeps_previous_when_the_store_breaks() {
        use crate::config_source::{empty_doc_hash, ConfigSource, DbSource};
        use lumen_auth::store::KeyStore;
        use wiremock::MockServer;

        let upstream = MockServer::start().await;
        let dir = tempdir();
        // DB mode's boot file holds ONLY the boot layer: auth must be
        // enabled (ADR 012 §1 requires a database), everything else -
        // providers included - comes from the DB document below.
        let boot_path = write_config(
            &dir,
            r"
            [auth]
            enabled = true
            ",
        );

        let store = KeyStore::in_memory().await.expect("store");
        let source = DbSource::new(store.clone());
        let doc = format!(
            r#"
            [[providers]]
            name = "openai"
            kind = "openai"
            base_url = "{}"
            [[providers.models]]
            id = "gpt"
            capabilities = ["chat"]
            "#,
            upstream.uri()
        );
        source
            .persist(&doc, &empty_doc_hash())
            .await
            .expect("persist the initial doc");

        let ctx = Arc::new(ConfigContext::db(boot_path, source));
        // Boot with an empty registry, as a fresh DB-mode process would if it
        // started before anything was ever persisted.
        let registry = Arc::new(
            Registry::build(
                Vec::new(),
                http::build_client(),
                std::time::Duration::from_secs(300),
            )
            .expect("empty registry"),
        );
        let metrics = Metrics::new();
        let t = Arc::new(targets(
            Arc::clone(&registry),
            ReloadMetrics::register(&metrics).unwrap(),
        ));

        reload_once(&ctx, &t).await;
        assert!(
            registry.chat_route("gpt").is_some(),
            "the persisted DB document's model is routable after one reload"
        );

        // Break the source: the config_versions table itself is gone, so
        // `DbSource::load` now fails outright.
        sqlx::query("DROP TABLE config_versions")
            .execute(store.pool())
            .await
            .expect("drop the config_versions table");

        reload_once(&ctx, &t).await;
        assert!(
            registry.chat_route("gpt").is_some(),
            "a broken DB source must keep the previous, working registry"
        );
        assert!(
            metrics
                .encode_text()
                .contains("lumen_config_reload_failures_total 1"),
            "the failed reload against the broken store must be counted"
        );
    }

    #[test]
    fn key_backfill_fills_only_env_keyless_providers() {
        use lumen_providers::{ProviderKind, ProviderSpec};
        let mut specs = vec![
            ProviderSpec {
                name: "from-env".to_owned(),
                kind: ProviderKind::Openai,
                api_key: Some("env-key".to_owned()), // already resolved from env
                base_url: None,
                api_version: None,
                strict: false,
                connect_timeout_ms: None,
                models: Vec::new(),
            },
            ProviderSpec {
                name: "from-db".to_owned(),
                kind: ProviderKind::Cohere,
                api_key: None, // env var unset → would go out unauthenticated
                base_url: None,
                api_version: None,
                strict: false,
                connect_timeout_ms: None,
                models: Vec::new(),
            },
        ];
        let mut backfill = HashMap::new();
        backfill.insert("from-db".to_owned(), "db-key".to_owned());
        // A stale entry for the env-keyed provider must NOT override env.
        backfill.insert("from-env".to_owned(), "should-not-win".to_owned());

        merge_key_backfill(&mut specs, &backfill);

        assert_eq!(specs[0].api_key.as_deref(), Some("env-key"), "env wins");
        assert_eq!(
            specs[1].api_key.as_deref(),
            Some("db-key"),
            "DB key re-applied so the reload doesn't strip it"
        );
    }

    /// Build a `notify` event of `kind` naming `path`, as the backend would.
    fn fs_event(kind: notify::EventKind, path: &Path) -> notify::Event {
        notify::Event::new(kind).add_path(path.to_path_buf())
    }

    /// The config file's own name, as `spawn_config_reloader` extracts it.
    fn config_name(path: &Path) -> Option<&std::ffi::OsStr> {
        path.file_name()
    }

    /// Regression test for the self-sustaining reload loop (v0.3.1 and
    /// earlier): the directory watch is armed with an inotify mask that
    /// includes `IN_OPEN` (0x20, part of the 0xfee `notify` arms), and the
    /// reload itself OPENS the config file to re-read it. Feeding that open
    /// back in as a reload trigger made every reload schedule the next one,
    /// ~4 reloads/second forever, clearable only by a restart. Any read of
    /// the file by any process (`cat`, `lumen --check-config`, `lumen keys
    /// list`) kicked it off.
    #[test]
    fn an_open_of_the_config_file_does_not_schedule_a_reload() {
        use notify::event::{AccessKind, AccessMode};
        let path = Path::new("/etc/lumen/config.toml");
        let open = fs_event(
            notify::EventKind::Access(AccessKind::Open(AccessMode::Any)),
            path,
        );
        assert!(
            !event_should_reload(&open, config_name(path)),
            "an open of the config file must not schedule a reload: the reload \
             itself opens the file, so reacting to opens is a feedback loop"
        );
    }

    /// The other non-mutating accesses a backend can report. None of them
    /// changed a byte, so none of them warrants a reload. The inotify mask
    /// happens to arm only `IN_OPEN` out of this set today, but the reload's
    /// own file handle is also *closed* read-only, so each of these is
    /// another latent way into the loop above if a backend ever reports it.
    #[test]
    fn reads_and_read_closes_of_the_config_file_do_not_schedule_a_reload() {
        use notify::event::{AccessKind, AccessMode};
        let path = Path::new("/etc/lumen/config.toml");
        for kind in [
            AccessKind::Read,
            AccessKind::Close(AccessMode::Read),
            AccessKind::Open(AccessMode::Read),
            AccessKind::Open(AccessMode::Execute),
            AccessKind::Any,
            AccessKind::Other,
        ] {
            let event = fs_event(notify::EventKind::Access(kind), path);
            assert!(
                !event_should_reload(&event, config_name(path)),
                "a non-mutating access ({kind:?}) must not schedule a reload"
            );
        }
    }

    /// The flip side: do not fix the loop by breaking hot reload. Every
    /// event kind that means "the bytes behind this path changed" must still
    /// schedule a reload. `Access(Close(Write))` is in the list because that
    /// is what the inotify backend maps `IN_CLOSE_WRITE` to: an `Access`
    /// event that nonetheless marks a completed write.
    #[test]
    fn content_and_rename_events_on_the_config_file_still_schedule_a_reload() {
        use notify::event::{
            AccessKind, AccessMode, CreateKind, DataChange, ModifyKind, RemoveKind, RenameMode,
        };
        let path = Path::new("/etc/lumen/config.toml");
        let kinds = [
            notify::EventKind::Modify(ModifyKind::Data(DataChange::Any)),
            notify::EventKind::Modify(ModifyKind::Name(RenameMode::To)),
            notify::EventKind::Modify(ModifyKind::Any),
            notify::EventKind::Access(AccessKind::Close(AccessMode::Write)),
            notify::EventKind::Create(CreateKind::File),
            notify::EventKind::Remove(RemoveKind::File),
            notify::EventKind::Any,
        ];
        for kind in kinds {
            let event = fs_event(kind, path);
            assert!(
                event_should_reload(&event, config_name(path)),
                "a content/rename event ({kind:?}) must still schedule a reload"
            );
        }
    }

    /// Path filtering is unchanged: churn on a neighbour in the config
    /// directory (the auth SQLite DB and its WAL, `lumen.env`, a staged
    /// temp file mid-rename) is not a config change.
    #[test]
    fn a_write_to_a_neighbour_file_does_not_schedule_a_reload() {
        use notify::event::{DataChange, ModifyKind};
        let path = Path::new("/etc/lumen/config.toml");
        for neighbour in [
            "/etc/lumen/lumen.env",
            "/etc/lumen/keys.db-wal",
            "/etc/lumen/config.toml.staged",
        ] {
            let event = fs_event(
                notify::EventKind::Modify(ModifyKind::Data(DataChange::Any)),
                Path::new(neighbour),
            );
            assert!(
                !event_should_reload(&event, config_name(path)),
                "{neighbour} is not the config file"
            );
        }
    }

    #[test]
    fn watch_target_falls_back_to_the_cwd_for_a_bare_filename() {
        // `lumen --config lumen.toml` (the quickstart form): no directory
        // component at all.
        assert_eq!(watch_target(Path::new("lumen.toml")), Path::new("."));
    }

    #[test]
    fn watch_target_uses_the_parent_directory_when_present() {
        assert_eq!(
            watch_target(Path::new("/etc/lumen/lumen.toml")),
            Path::new("/etc/lumen")
        );
    }

    // The regression test for a bare-filename config path surviving a
    // rename-replace (`spawn_config_reloader_survives_a_rename_replace_of_a_bare_filename_config`)
    // used to live here, but it calls `std::env::set_current_dir` and holds
    // a foreign working directory for the better part of a second. Two
    // tests in `crates/server/src/config.rs` (`env_var_overrides_file_value`,
    // `master_key_env_var_is_never_folded_into_the_config`) use
    // `figment::Jail`, which chdirs internally and serialises only against
    // OTHER jails via its own private static lock - it has no way to know
    // about a chdir happening outside of it. Sharing this lib's unit test
    // binary (and therefore a process and a CWD) with those tests made the
    // chdir here liable to land in the middle of a jail test's relative-path
    // `Config::load`, and made this test's CWD-restoring guard liable to
    // capture a jail's temp directory as "the original CWD" and later
    // restore the process into a directory that had since been deleted:
    // a real, if intermittent, source of CI flakiness. It now lives in
    // `crates/server/tests/reload.rs`, which Cargo builds and runs as its
    // own process, making that interference structurally impossible instead
    // of relying on a lock every CWD-touching test would have to remember
    // to take.

    /// A unique temp dir under the OS temp root (no external crate).
    fn tempdir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        // A monotonic-ish unique suffix without Instant/rand: an atomic counter
        // plus the pid.
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let base = std::env::temp_dir();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let dir = base.join(format!("lumen-reload-test-{pid}-{n}"));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }
}
