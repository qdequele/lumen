//! Configuration hot reload (M7 §7.3).
//!
//! On `SIGHUP` or a change to the config file, the config is re-loaded and
//! **validated**; only if it is valid does the provider routing table swap
//! atomically via the registry's [`ArcSwap`](ferrogate_providers::Registry).
//! In-flight requests hold a snapshot of the old table (`.load()`), so the swap
//! never disturbs them. An invalid reload is logged, the
//! `ferrogate_config_reload_failures_total` metric is incremented, and the
//! previous configuration is kept (criterion 3).
//!
//! Scope: the reload swaps the **routing table** (providers, models, aliases,
//! fallbacks resolve against it). Server bind address, auth, pricing and the
//! resilience runtime are read once at boot and are *not* hot-reloaded — those
//! still require a restart (noted in `docs/backlog.md`). Provider API keys are
//! re-resolved from the environment only; DB-stored keys remain boot-time.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ferrogate_providers::{Registry, RegistryError};
use ferrogate_telemetry::ReloadMetrics;

use crate::config::{Config, ConfigError};

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

/// Re-load `path`, validate it, and (only on success) atomically swap the
/// registry's routing table. Increments the success/failure counters. On any
/// error the registry is left exactly as it was.
///
/// # Errors
/// [`ReloadError`] if the file is missing/invalid or the registry rebuild
/// fails; the running config is unaffected in both cases.
pub fn apply_reload(
    path: &Path,
    registry: &Registry,
    metrics: &ReloadMetrics,
) -> Result<(), ReloadError> {
    // `Config::load` parses AND validates; a bad file never reaches `reload`.
    let config = match Config::load(path) {
        Ok(config) => config,
        Err(error) => {
            metrics.inc_failure();
            tracing::warn!(%error, "config reload rejected; keeping the running config");
            return Err(error.into());
        }
    };
    // The registry rebuild is the last line of defence (duplicate model ids
    // etc. are already caught by validation, but a keyless provider missing a
    // base_url would surface here). A failure leaves the old table in place.
    if let Err(error) = registry.reload(config.provider_specs()) {
        metrics.inc_failure();
        tracing::warn!(%error, "config reload rejected by registry; keeping the running config");
        return Err(error.into());
    }
    metrics.inc_success();
    tracing::info!(
        model_count = config.loaded_models().len(),
        provider_count = config.providers.len(),
        "configuration reloaded; routing table swapped"
    );
    Ok(())
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
    registry: Arc<Registry>,
    metrics: ReloadMetrics,
) -> Result<tokio::task::JoinHandle<()>, notify::Error> {
    use notify::{RecursiveMode, Watcher};

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if res.is_ok() {
            // Non-blocking; a full/closed channel just drops the tick (the next
            // event, or the debounce drain, still triggers a reload).
            let _ = tx.send(());
        }
    })?;
    // Watch the parent directory: many editors replace the file (rename), which
    // stops a watch on the file itself from firing. Fall back to the file when
    // it has no parent component.
    let watch_target = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map_or(path.as_path(), |p| p);
    watcher.watch(watch_target, RecursiveMode::NonRecursive)?;

    let handle = tokio::spawn(async move {
        // Keep the watcher alive for the lifetime of the task.
        let _watcher = watcher;
        let mut sighup = hangup_signal();
        loop {
            tokio::select! {
                () = wait_for_hangup(&mut sighup) => {
                    tracing::info!("SIGHUP received; reloading config");
                    let _ = apply_reload(&path, &registry, &metrics);
                }
                event = rx.recv() => {
                    if event.is_none() {
                        break; // sender dropped (never, in practice)
                    }
                    // Coalesce the rest of the burst before reloading.
                    tokio::time::sleep(DEBOUNCE).await;
                    while rx.try_recv().is_ok() {}
                    let _ = apply_reload(&path, &registry, &metrics);
                }
            }
        }
    });
    Ok(handle)
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
    use ferrogate_providers::http;
    use ferrogate_telemetry::Metrics;
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

    #[test]
    fn valid_reload_swaps_the_routing_table() {
        let dir = tempdir();
        let path = write_config(&dir, ONE_MODEL);
        let registry = registry_from(&path);
        assert!(registry.chat_route("gpt").is_some());
        assert!(registry.embedding_route("embed").is_none());

        let metrics = ReloadMetrics::register(&Metrics::new()).unwrap();
        write_config(&dir, TWO_MODELS);
        apply_reload(&path, &registry, &metrics).expect("valid reload");

        // The new model is now routable — the swap took effect.
        assert!(registry.embedding_route("embed").is_some());
        assert!(registry.knows_model("gpt"));
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
        let err = apply_reload(&path, &registry, &reload).unwrap_err();
        assert!(matches!(err, ReloadError::Config(_)));

        // Old routing table intact: the pre-reload models still resolve.
        assert!(registry.embedding_route("embed").is_some());
        assert!(registry.chat_route("gpt").is_some());
        // Failure counted, no success.
        let out = metrics.encode_text();
        assert!(out.contains("ferrogate_config_reload_failures_total 1"));
        assert!(out.contains("ferrogate_config_reloads_total 0"));
    }

    #[test]
    fn reload_of_a_deleted_file_is_rejected_and_keeps_the_table() {
        let dir = tempdir();
        let path = write_config(&dir, ONE_MODEL);
        let registry = registry_from(&path);
        let metrics = Metrics::new();
        let reload = ReloadMetrics::register(&metrics).unwrap();

        std::fs::remove_file(&path).expect("remove config");
        let err = apply_reload(&path, &registry, &reload).unwrap_err();
        assert!(matches!(
            err,
            ReloadError::Config(ConfigError::NotFound { .. })
        ));
        assert!(registry.chat_route("gpt").is_some(), "old table kept");
        assert!(metrics
            .encode_text()
            .contains("ferrogate_config_reload_failures_total 1"));
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
        let dir = base.join(format!("ferrogate-reload-test-{pid}-{n}"));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }
}
