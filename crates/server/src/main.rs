//! LUMEN server entry point.
//!
//! Thin orchestration only: parse args, load config, initialise logging, then
//! hand off to the library. `anyhow` is used here (and only here).

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::Context;
use arc_swap::ArcSwap;
use lumen_auth::crypto::MasterKey;
use lumen_auth::key::hash_key;
use lumen_auth::state::AuthState;
use lumen_auth::store::KeyStore;
use lumen_auth::usage::{spawn_usage_writer, UsageWriterConfig};
use lumen_server::{
    auth::{now_unix, AuthRuntime},
    build_app,
    config::Config,
    health::{spawn_health_checks, ProbeTarget, ProviderHealth},
    lifecycle, log_startup,
    pricing::CostTable,
    reload::{spawn_config_reloader, AuthKnobs, ProviderKeySource, ReloadTargets},
    resilience::ResilienceRuntime,
    state::AppState,
};
use lumen_telemetry::{
    logging::init_logging, LatencyMetrics, Metrics, ReloadMetrics, ResilienceMetrics, TokenMetrics,
};
use std::sync::Arc;
use tokio::net::TcpListener;

/// Env var holding the master key (64 hex chars): admin-API token and
/// at-rest encryption key for stored provider keys. Required when
/// `auth.enabled = true`. The value itself is never logged or stored.
const MASTER_KEY_ENV: &str = "LUMEN_MASTER_KEY";

/// How long to drain in-flight requests after a shutdown signal.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

const HELP: &str = "\
lumen - universal LLM gateway

USAGE:
    lumen [--config <PATH>]
    lumen --check-config [--config <PATH>]

OPTIONS:
    -c, --config <PATH>    Path to the TOML config file [default: config.toml]
    --check-config         Validate the config and exit: 0 if valid, non-zero
                            otherwise. Binds no listener, opens no database,
                            contacts no provider - safe for CI / deploy
                            pipelines to run ahead of a real boot.
    -h, --help             Print this help
";

fn main() -> ExitCode {
    let action = match parse_args() {
        Ok(action) => action,
        Err(message) => {
            eprintln!("error: {message}\n\n{HELP}");
            return ExitCode::from(2);
        }
    };

    let config_path = match action {
        Action::Help => return ExitCode::SUCCESS,
        Action::CheckConfig(path) => return run_check_config(&path),
        Action::Serve(path) => path,
    };

    // Load and validate config BEFORE the async runtime so a bad config exits
    // fast with a precise, operator-facing message (never a stack trace).
    let config = match Config::load(&config_path) {
        Ok(cfg) => cfg,
        Err(err) => {
            eprintln!("configuration error: {err}");
            return ExitCode::FAILURE;
        }
    };

    // Logging is initialised only after config parses, so the format is known.
    init_logging(config.log_format.into(), "info");

    match run(config, config_path) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!(error = %format!("{err:#}"), "server exited with error");
            ExitCode::FAILURE
        }
    }
}

/// What `main` should do, once CLI args are parsed.
#[derive(Debug, PartialEq, Eq)]
enum Action {
    /// Serve using the config at this path.
    Serve(PathBuf),
    /// `--check-config`: validate the config at this path and exit.
    CheckConfig(PathBuf),
    /// `-h`/`--help`: help was already printed.
    Help,
}

/// Parse `--config`/`-c`, `--check-config` and `--help`/`-h`.
fn parse_args() -> Result<Action, String> {
    parse_args_from(std::env::args().skip(1))
}

/// Parse an explicit argument list (excludes `argv[0]`); split out from
/// [`parse_args`] so the parsing logic is unit-testable without touching the
/// real process arguments.
fn parse_args_from(mut args: impl Iterator<Item = String>) -> Result<Action, String> {
    let mut config_path = PathBuf::from("config.toml");
    let mut check_config = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{HELP}");
                return Ok(Action::Help);
            }
            "-c" | "--config" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--config requires a path argument".to_owned())?;
                config_path = PathBuf::from(value);
            }
            "--check-config" => {
                check_config = true;
            }
            other => {
                if let Some(value) = other.strip_prefix("--config=") {
                    config_path = PathBuf::from(value);
                } else {
                    return Err(format!("unexpected argument '{other}'"));
                }
            }
        }
    }
    Ok(if check_config {
        Action::CheckConfig(config_path)
    } else {
        Action::Serve(config_path)
    })
}

/// `--check-config`: validate `config_path` and print a clear success or
/// failure message. Exits 0 when the config is valid, non-zero otherwise.
/// Delegates to [`lumen_server::check_config`], which stays local-only (no
/// listener, no database, no provider contacted) so it is safe for CI /
/// deploy pipelines to run ahead of a real boot.
fn run_check_config(config_path: &Path) -> ExitCode {
    match lumen_server::check_config(config_path) {
        Ok(report) => {
            println!(
                "config OK: {} ({} provider(s), {} model(s))",
                config_path.display(),
                report.provider_count,
                report.model_count
            );
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("configuration error: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Build the app and serve until shutdown. Uses its own multi-thread runtime.
/// Build the guarded image-fetch policy from config, logging its posture once
/// at boot (a warning when enabled with no host/prefix allowlist).
fn build_image_fetch_policy(
    config: &Config,
) -> std::sync::Arc<lumen_providers::image_fetch::ImageFetchPolicy> {
    if config.image_fetch.enabled {
        if config.image_fetch.is_unrestricted() {
            tracing::warn!(
                "image fetch enabled with no host/prefix allowlist; only scheme and private-IP guards apply"
            );
        } else {
            tracing::info!("image fetch enabled (host/prefix allowlist active)");
        }
    }
    std::sync::Arc::new(config.image_fetch.to_policy())
}

fn run(config: Config, config_path: PathBuf) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to build tokio runtime")?;

    runtime.block_on(async move {
        log_startup(&config);

        let addr = format!("{}:{}", config.server.host, config.server.port);
        let listener = TcpListener::bind(&addr)
            .await
            .with_context(|| format!("failed to bind {addr}"))?;
        tracing::info!(%addr, "listening");

        let guards = lumen_server::StreamGuards {
            first_token_timeout: Duration::from_millis(config.server.first_token_timeout_ms),
            heartbeat_interval: Duration::from_millis(config.server.sse_heartbeat_ms),
        };
        let metrics = Metrics::new();
        let tokens = TokenMetrics::register(&metrics, &config.telemetry.metadata_labels)
            .context("failed to register token metrics")?;
        let latency =
            LatencyMetrics::register(&metrics).context("failed to register latency metrics")?;
        let resilience_metrics = ResilienceMetrics::register(&metrics)
            .context("failed to register resilience metrics")?;
        let reload_metrics =
            ReloadMetrics::register(&metrics).context("failed to register reload metrics")?;
        let resilience = Arc::new(ResilienceRuntime::from_config(
            &config,
            Some(resilience_metrics.clone()),
        ));

        // The M5 auth stack: SQLite store, in-memory key state, usage writer,
        // periodic budget flush and retention purge. All optional - the
        // gateway stays a stateless open proxy when auth is disabled.
        let mut provider_specs = config.provider_specs();
        let (auth_runtime, usage_logger, usage_writer, boot_backfill, key_source, auth_knobs) =
            if config.auth.enabled {
                let boot = boot_auth_stack(&config, &mut provider_specs).await?;
                (
                    Some(boot.runtime),
                    Some(boot.usage_logger),
                    Some(boot.usage_writer),
                    boot.key_backfill,
                    Some(boot.key_source),
                    Some(boot.auth_knobs),
                )
            } else {
                (
                    None,
                    None,
                    None,
                    std::collections::HashMap::new(),
                    None,
                    None,
                )
            };

        // The shared client sets the default (process-wide) connect timeout and
        // an overall cap that backstops the executor's total timeout (M6 §6.4).
        // A provider that sets `connect_timeout_ms` gets its own client with the
        // same overall backstop (ADR 005, 2026-07-15 amendment); every other
        // provider keeps sharing this pooled client.
        let overall_backstop =
            Duration::from_millis(config.resilience.total_timeout_ms.saturating_add(30_000));
        let client = lumen_providers::http::build_client_with(
            Duration::from_millis(config.resilience.connect_timeout_ms),
            overall_backstop,
        );
        let registry = Arc::new(
            lumen_providers::Registry::build(provider_specs, client.clone(), overall_backstop)
                .context("failed to build provider registry")?,
        );

        // Shared cell so the hot reloader swaps the very cell handlers read.
        let pricing = Arc::new(ArcSwap::from_pointee(CostTable::from_config(&config)));

        // Config hot reload (M7 §7.3): SIGHUP / file change / admin trigger swaps
        // routing, pricing, resilience and auth knobs and re-reads DB provider
        // keys (rotation without restart). See `reload` module docs.
        let reload_trigger = Arc::new(tokio::sync::Notify::new());
        let reload_targets = ReloadTargets {
            registry: Arc::clone(&registry),
            pricing: Arc::clone(&pricing),
            resilience: Arc::clone(&resilience),
            metrics: reload_metrics,
            key_backfill: Arc::new(ArcSwap::from_pointee(boot_backfill)),
            key_source,
            auth_knobs,
        };
        let reload_armed = arm_config_reload(config_path, reload_targets, &reload_trigger);

        let health = boot_health(&config, &client, &resilience_metrics);

        let image_fetch = build_image_fetch_policy(&config);

        let mut state = AppState::new(metrics, registry, tokens, latency)
            .with_guards(guards)
            .with_pricing_cell(pricing)
            .with_resilience(resilience)
            .with_health(health)
            .with_body_limit(config.server.body_limit)
            .with_image_fetch(image_fetch);
        // Expose the reload trigger only when the reloader is actually armed.
        if reload_armed {
            state = state.with_reload_trigger(Arc::clone(&reload_trigger));
        }
        if let Some(runtime) = auth_runtime.clone() {
            state = state.with_auth(runtime);
        }
        if let Some(logger) = usage_logger {
            state = state.with_usage(logger);
        }
        let app = build_app(state);

        lifecycle::serve(listener, app, DRAIN_TIMEOUT, lifecycle::shutdown_signal())
            .await
            .context("server error")?;

        drain_on_shutdown(auth_runtime, usage_writer).await;

        tracing::info!("shutdown complete");
        Ok(())
    })
}

/// Clean-shutdown drain: a final budget flush (so a clean shutdown loses zero
/// accounting) then a bounded wait for the usage writer to flush and exit.
/// `serve` returning dropped the app (and every `UsageLogger` clone), closing
/// the channel; the wait is bounded so shutdown never hangs on a sick database.
async fn drain_on_shutdown(
    auth_runtime: Option<Arc<AuthRuntime>>,
    usage_writer: Option<tokio::task::JoinHandle<()>>,
) {
    if let Some(runtime) = auth_runtime {
        let dirty = runtime.keys.drain_dirty();
        if !dirty.is_empty() {
            if let Err(error) = runtime.store.persist_budgets(&dirty).await {
                tracing::warn!(%error, "final budget flush failed");
            }
        }
    }
    if let Some(writer) = usage_writer {
        if tokio::time::timeout(Duration::from_secs(5), writer)
            .await
            .is_err()
        {
            tracing::warn!("usage writer did not drain within 5s; giving up");
        }
    }
}

/// Arm the config hot reloader (SIGHUP + file watch + admin trigger). A
/// watcher-setup failure only disables reload - the server still runs - so it
/// is logged, not fatal. Returns whether the reloader is armed, so the caller
/// only exposes the admin reload trigger when a reload can actually happen.
fn arm_config_reload(
    config_path: PathBuf,
    targets: ReloadTargets,
    trigger: &Arc<tokio::sync::Notify>,
) -> bool {
    match spawn_config_reloader(config_path, targets, Arc::clone(trigger)) {
        Ok(_handle) => {
            tracing::info!("config hot reload armed (SIGHUP + file watch + admin trigger)");
            true
        }
        Err(error) => {
            tracing::warn!(%error, "config hot reload unavailable");
            false
        }
    }
}

/// Seed the provider-health registry (every provider `unknown`) and, when
/// enabled, spawn the background probe task (M6 §6.5). Only providers with a
/// configured `base_url` are probed; vendor-default URLs stay `unknown`.
fn boot_health(
    config: &Config,
    client: &reqwest::Client,
    resilience_metrics: &ResilienceMetrics,
) -> Arc<ProviderHealth> {
    let provider_names: Vec<String> = config.providers.iter().map(|p| p.name.clone()).collect();
    let health = Arc::new(ProviderHealth::with_providers(&provider_names));
    if !config.resilience.health_check_enabled {
        return health;
    }
    let targets: Vec<ProbeTarget> = config
        .providers
        .iter()
        .filter_map(|p| {
            p.base_url.clone().map(|url| ProbeTarget {
                name: p.name.clone(),
                url,
                kind: p.kind,
            })
        })
        .collect();
    if targets.is_empty() {
        tracing::warn!(
            "health checks enabled but no provider has a configured base_url; providers on \
             built-in vendor URLs report 'unknown' (never probed)"
        );
    } else {
        spawn_health_checks(
            client.clone(),
            targets,
            Arc::clone(&health),
            Some(resilience_metrics.clone()),
            Duration::from_millis(config.resilience.health_check_interval_ms),
            Duration::from_millis(config.resilience.connect_timeout_ms),
        );
    }
    health
}

/// Everything [`boot_auth_stack`] hands back to [`run`].
struct AuthBoot {
    /// The virtual-key auth runtime (in-memory table, store, admin token).
    runtime: Arc<AuthRuntime>,
    /// The usage-log channel handle exposed to the request path.
    usage_logger: lumen_auth::usage::UsageLogger,
    /// The usage writer task (drained on shutdown).
    usage_writer: tokio::task::JoinHandle<()>,
    /// Boot-time DB provider-key snapshot (seeds the hot-reload backfill cell).
    key_backfill: std::collections::HashMap<String, String>,
    /// DB key source the reloader re-reads on every reload (rotation support).
    key_source: Arc<ProviderKeySource>,
    /// Live auth knobs the flush/purge tasks read and a reload retunes.
    auth_knobs: Arc<AuthKnobs>,
}

/// Boot the M5 auth stack: master key, SQLite store, provider-key back-fill,
/// in-memory key table, usage writer, periodic budget flush and retention
/// purge. The flush and purge tasks read their cadence/window from the shared
/// [`AuthKnobs`] so a hot reload retunes them with no restart.
async fn boot_auth_stack(
    config: &Config,
    provider_specs: &mut [lumen_providers::ProviderSpec],
) -> anyhow::Result<AuthBoot> {
    use zeroize::Zeroize;
    let mut master_value = std::env::var(MASTER_KEY_ENV).with_context(|| {
        format!("auth.enabled requires the {MASTER_KEY_ENV} env var (64 hex chars)")
    })?;
    let master = MasterKey::from_env_value(&master_value)
        .with_context(|| format!("invalid {MASTER_KEY_ENV}"))?;
    // A live knob cell shared with the flush/purge tasks and swapped on reload.
    let auth_knobs = Arc::new(AuthKnobs::from_config(config));

    let store = KeyStore::connect(&config.auth.db_url())
        .await
        .with_context(|| format!("failed to open auth database '{}'", config.auth.db_path))?;

    // Provider keys stored encrypted in the DB back-fill any provider whose
    // env var is unset (env vars stay the primary source). The snapshot is
    // handed to the hot-reload path so a reload re-applies these keys instead
    // of stripping them (they are absent from the env-only spec rebuild).
    let mut key_backfill = std::collections::HashMap::new();
    for spec in provider_specs.iter_mut() {
        if spec.api_key.is_none() {
            if let Some(key) = store
                .load_provider_key(&spec.name, &master)
                .await
                .with_context(|| format!("failed to decrypt stored provider key '{}'", spec.name))?
            {
                spec.api_key = Some(key.clone());
                key_backfill.insert(spec.name.clone(), key);
            }
        }
    }

    let entries = store
        .load_auth_entries()
        .await
        .context("failed to load virtual keys")?;
    let keys = AuthState::load(entries);
    tracing::info!(key_count = keys.len(), "virtual keys loaded");

    let (logger, writer) = spawn_usage_writer(
        store.clone(),
        UsageWriterConfig {
            capacity: config.auth.usage_channel_capacity,
            batch_max: config.auth.usage_batch_max,
            flush_interval: Duration::from_millis(config.auth.usage_flush_ms),
        },
    );

    // The reloader re-reads DB provider keys on each reload (rotation without a
    // restart). It needs its own master handle (the runtime's is moved in
    // below), built here before the clear master string is wiped.
    let provider_names: Vec<String> = config.providers.iter().map(|p| p.name.clone()).collect();
    let source_master = MasterKey::from_env_value(&master_value)
        .with_context(|| format!("invalid {MASTER_KEY_ENV}"))?;
    let key_source = Arc::new(ProviderKeySource::new(
        store.clone(),
        source_master,
        provider_names,
    ));

    let runtime = Arc::new(AuthRuntime {
        keys,
        store: store.clone(),
        admin_token_hash: hash_key(&master_value),
        master: Some(master),
    });
    // The raw master key is now only needed as the redacted `MasterKey` bytes
    // (zeroized on drop) and the one-way admin hash; wipe the clear copy.
    master_value.zeroize();

    // Periodic budget flush: memory → DB. A crash loses at most one interval
    // of *accounting*; enforcement lives in memory and is reloaded from the
    // last flush at boot. The cadence is read live from `auth_knobs` so a hot
    // reload retunes it without a restart (a sleep loop, since a live period
    // change can't be pushed into a fixed `tokio::time::Interval`).
    let flush_runtime = Arc::clone(&runtime);
    let flush_knobs = Arc::clone(&auth_knobs);
    tokio::spawn(async move {
        loop {
            // `.max(1)` guards a reload that disabled auth (knob validated to be
            // non-zero while auth is enabled, but a disabling reload sets 0).
            let interval = Duration::from_millis(flush_knobs.flush_interval_ms().max(1));
            tokio::time::sleep(interval).await;
            let dirty = flush_runtime.keys.drain_dirty();
            if dirty.is_empty() {
                continue;
            }
            if let Err(error) = flush_runtime.store.persist_budgets(&dirty).await {
                tracing::warn!(%error, "budget flush failed; will retry next interval");
            }
        }
    });

    // Retention purge: drop usage_log rows older than the window. The window is
    // read live from `auth_knobs` each tick so a reload retunes it in place.
    let purge_knobs = Arc::clone(&auth_knobs);
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(3_600));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let retention = i64::from(purge_knobs.retention_days()) * 86_400;
            match store.purge_usage_older_than(now_unix() - retention).await {
                Ok(0) => {}
                Ok(purged) => tracing::info!(purged, "usage-log retention purge"),
                Err(error) => tracing::warn!(%error, "usage-log purge failed"),
            }
        }
    });

    Ok(AuthBoot {
        runtime,
        usage_logger: logger,
        usage_writer: writer,
        key_backfill,
        key_source,
        auth_knobs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> impl Iterator<Item = String> {
        values
            .iter()
            .map(|s| (*s).to_owned())
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn no_args_serves_the_default_config_path() {
        let action = parse_args_from(args(&[])).expect("no args should parse");
        assert_eq!(action, Action::Serve(PathBuf::from("config.toml")));
    }

    #[test]
    fn short_config_flag_sets_the_path() {
        let action = parse_args_from(args(&["-c", "custom.toml"])).expect("-c should parse");
        assert_eq!(action, Action::Serve(PathBuf::from("custom.toml")));
    }

    #[test]
    fn long_config_flag_with_equals_sets_the_path() {
        let action =
            parse_args_from(args(&["--config=custom.toml"])).expect("--config= should parse");
        assert_eq!(action, Action::Serve(PathBuf::from("custom.toml")));
    }

    #[test]
    fn missing_config_value_is_an_error() {
        let err = parse_args_from(args(&["--config"])).expect_err("bare --config must error");
        assert!(err.contains("--config requires a path argument"));
    }

    #[test]
    fn unexpected_argument_is_an_error() {
        let err = parse_args_from(args(&["--bogus"])).expect_err("unknown flag must error");
        assert!(err.contains("--bogus"));
    }

    #[test]
    fn check_config_flag_defaults_to_the_default_config_path() {
        let action =
            parse_args_from(args(&["--check-config"])).expect("--check-config should parse");
        assert_eq!(action, Action::CheckConfig(PathBuf::from("config.toml")));
    }

    #[test]
    fn check_config_flag_combines_with_an_explicit_config_path() {
        let action = parse_args_from(args(&["--check-config", "-c", "ci.toml"]))
            .expect("--check-config with -c should parse");
        assert_eq!(action, Action::CheckConfig(PathBuf::from("ci.toml")));

        // Order independence: the path flag may come first too.
        let action = parse_args_from(args(&["--config=ci.toml", "--check-config"]))
            .expect("-c with --check-config should parse");
        assert_eq!(action, Action::CheckConfig(PathBuf::from("ci.toml")));
    }

    #[test]
    fn help_flag_wins_and_returns_help() {
        let action = parse_args_from(args(&["--check-config", "-h"])).expect("-h should parse");
        assert_eq!(action, Action::Help);
    }
}
