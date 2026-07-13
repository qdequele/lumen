//! Configuration hot reload (M7 §7.3).
//!
//! On `SIGHUP` or a change to the config file, the config is re-loaded and
//! **validated**; only if it is valid does the provider routing table swap
//! atomically via the registry's [`ArcSwap`](lumen_providers::Registry).
//! In-flight requests hold a snapshot of the old table (`.load()`), so the swap
//! never disturbs them. An invalid reload is logged, the
//! `lumen_config_reload_failures_total` metric is incremented, and the
//! previous configuration is kept (criterion 3).
//!
//! Scope: the reload swaps the **routing table** (providers, models, aliases,
//! fallbacks resolve against it). Server bind address, auth, pricing and the
//! resilience runtime are read once at boot and are *not* hot-reloaded — those
//! still require a restart (noted in `docs/backlog.md`).
//!
//! Provider API keys are re-resolved from the environment on every reload (env
//! stays the primary source). Keys that were back-filled from the encrypted DB
//! at boot are preserved across reloads via a snapshot captured at boot and
//! merged in here — so a reload never silently drops a DB-stored key and sends
//! unauthenticated upstream requests. Rotating a DB key still needs a restart
//! (the snapshot is boot-time; noted in `docs/backlog.md`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use lumen_providers::{Registry, RegistryError};
use lumen_telemetry::ReloadMetrics;

use crate::config::{Config, ConfigError};
use crate::pricing::CostTable;
use crate::resilience::ResilienceRuntime;

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

/// The process-wide handles a reload swaps: the routing table, the price table
/// and the resilience policy (the circuit breakers inside `resilience` are
/// deliberately *not* swapped — their live state survives the reload). Bundled
/// so the reload signature stays small and future config surfaces can join.
pub struct ReloadTargets {
    /// The provider routing table (its own `ArcSwap` inside).
    pub registry: Arc<Registry>,
    /// The price table cell (DEBT-1).
    pub pricing: Arc<ArcSwap<CostTable>>,
    /// The resilience runtime; only its policy cell is swapped.
    pub resilience: Arc<ResilienceRuntime>,
    /// Reload success/failure counters.
    pub metrics: ReloadMetrics,
    /// Boot-time DB provider-key snapshot, re-applied so a reload never strips
    /// a stored key (env still wins).
    pub key_backfill: HashMap<String, String>,
}

/// Re-load `path`, validate it, and (only on success) atomically swap the
/// routing table, price table and resilience policy. Increments the
/// success/failure counters. On any error every target is left exactly as it
/// was (the fallible registry rebuild runs first, before any swap).
///
/// # Errors
/// [`ReloadError`] if the file is missing/invalid or the registry rebuild
/// fails; the running config is unaffected in both cases.
pub fn apply_reload(path: &Path, targets: &ReloadTargets) -> Result<(), ReloadError> {
    // `Config::load` parses AND validates; a bad file never reaches `reload`.
    let config = match Config::load(path) {
        Ok(config) => config,
        Err(error) => {
            targets.metrics.inc_failure();
            tracing::warn!(%error, "config reload rejected; keeping the running config");
            return Err(error.into());
        }
    };
    // `provider_specs` resolves keys from the environment; re-apply the boot
    // DB-key snapshot for any provider still keyless, mirroring boot back-fill
    // so a reload never strips a DB-stored key (env keeps precedence).
    let mut specs = config.provider_specs();
    merge_key_backfill(&mut specs, &targets.key_backfill);
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
        .store(Arc::new(CostTable::from_config(&config)));
    targets.resilience.reload_policy(&config);
    targets.metrics.inc_success();
    tracing::info!(
        model_count = config.loaded_models().len(),
        provider_count = config.providers.len(),
        "configuration reloaded; routing table, pricing and resilience policy swapped"
    );
    Ok(())
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

/// Spawn the background reloader: reload on `SIGHUP` and on changes to the
/// config file. The returned task runs until the process exits; the file
/// watcher is kept alive inside it.
///
/// # Errors
/// Returns the `notify` error if the file watcher cannot be created or armed;
/// the caller should log it and continue (hot reload via SIGHUP still works if
/// the watcher fails — but here both share the watcher setup, so a failure
/// disables both and is surfaced to the caller).
pub fn spawn_config_reloader(
    path: PathBuf,
    targets: ReloadTargets,
) -> Result<tokio::task::JoinHandle<()>, notify::Error> {
    use notify::{RecursiveMode, Watcher};

    // Watch the parent directory (editors replace the file via rename, which a
    // watch on the file itself would miss), but only react to events that touch
    // the config file — a neighbour file (e.g. the SQLite DB) must not trigger
    // a reload. Matching by file name avoids canonicalize races when the file
    // is briefly absent mid-rename.
    let config_name = path.file_name().map(std::ffi::OsStr::to_owned);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res {
            let touches_config = event
                .paths
                .iter()
                .any(|p| p.file_name() == config_name.as_deref());
            if touches_config {
                // Non-blocking; a full/closed channel just drops the tick (the
                // next event, or the debounce drain, still triggers a reload).
                let _ = tx.send(());
            }
        }
    })?;
    let watch_target = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map_or(path.as_path(), |p| p);
    watcher.watch(watch_target, RecursiveMode::NonRecursive)?;

    let targets = Arc::new(targets);
    let handle = tokio::spawn(async move {
        // Keep the watcher alive for the lifetime of the task.
        let _watcher = watcher;
        let mut sighup = hangup_signal();
        loop {
            tokio::select! {
                () = wait_for_hangup(&mut sighup) => {
                    tracing::info!("SIGHUP received; reloading config");
                    reload_blocking(&path, &targets).await;
                }
                event = rx.recv() => {
                    if event.is_none() {
                        break; // sender dropped (never, in practice)
                    }
                    // Coalesce the rest of the burst before reloading.
                    tokio::time::sleep(DEBOUNCE).await;
                    while rx.try_recv().is_ok() {}
                    reload_blocking(&path, &targets).await;
                }
            }
        }
    });
    Ok(handle)
}

/// Run [`apply_reload`] on a blocking thread (it does synchronous figment file
/// I/O), so the runtime worker is never blocked (CLAUDE.md rule 2 in spirit).
async fn reload_blocking(path: &Path, targets: &Arc<ReloadTargets>) {
    let path = path.to_path_buf();
    let targets = Arc::clone(targets);
    let joined = tokio::task::spawn_blocking(move || {
        let _ = apply_reload(&path, &targets);
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
        Arc::new(Registry::build(config.provider_specs(), http::build_client()).expect("registry"))
    }

    /// Reload targets sharing `registry`/`metrics`, with default pricing and
    /// resilience and no key backfill.
    fn targets(registry: Arc<Registry>, metrics: ReloadMetrics) -> ReloadTargets {
        ReloadTargets {
            registry,
            pricing: Arc::new(ArcSwap::from_pointee(CostTable::default())),
            resilience: Arc::new(ResilienceRuntime::defaults()),
            metrics,
            key_backfill: HashMap::new(),
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
        apply_reload(&path, &t).expect("valid reload");

        // The new model is now routable — the swap took effect.
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
        apply_reload(&path, &t).expect("valid reload");

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
    fn invalid_reload_keeps_the_old_table_and_counts_the_failure() {
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
        let t = targets(Arc::clone(&registry), reload);
        let err = apply_reload(&path, &t).unwrap_err();
        assert!(matches!(err, ReloadError::Config(_)));

        // Old routing table intact: the pre-reload models still resolve.
        assert!(registry.embedding_route("embed").is_some());
        assert!(registry.chat_route("gpt").is_some());
        // Failure counted, no success.
        let out = metrics.encode_text();
        assert!(out.contains("lumen_config_reload_failures_total 1"));
        assert!(out.contains("lumen_config_reloads_total 0"));
    }

    #[test]
    fn reload_of_a_deleted_file_is_rejected_and_keeps_the_table() {
        let dir = tempdir();
        let path = write_config(&dir, ONE_MODEL);
        let registry = registry_from(&path);
        let metrics = Metrics::new();
        let reload = ReloadMetrics::register(&metrics).unwrap();

        std::fs::remove_file(&path).expect("remove config");
        let t = targets(Arc::clone(&registry), reload);
        let err = apply_reload(&path, &t).unwrap_err();
        assert!(matches!(
            err,
            ReloadError::Config(ConfigError::NotFound { .. })
        ));
        assert!(registry.chat_route("gpt").is_some(), "old table kept");
        assert!(metrics
            .encode_text()
            .contains("lumen_config_reload_failures_total 1"));
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
                models: Vec::new(),
            },
            ProviderSpec {
                name: "from-db".to_owned(),
                kind: ProviderKind::Cohere,
                api_key: None, // env var unset → would go out unauthenticated
                base_url: None,
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
