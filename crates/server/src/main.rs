//! LUMEN server entry point.
//!
//! Thin orchestration only: parse args, load config, initialise logging, then
//! hand off to the library. `anyhow` is used here (and only here).

use std::path::PathBuf;
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
    reload::{spawn_config_reloader, ReloadTargets},
    resilience::ResilienceRuntime,
    state::AppState,
};
use lumen_telemetry::{
    logging::init_logging, Metrics, ReloadMetrics, ResilienceMetrics, TokenMetrics,
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

OPTIONS:
    -c, --config <PATH>    Path to the TOML config file [default: config.toml]
    -h, --help             Print this help
";

fn main() -> ExitCode {
    let config_path = match parse_args() {
        Ok(Some(path)) => path,
        Ok(None) => return ExitCode::SUCCESS, // --help
        Err(message) => {
            eprintln!("error: {message}\n\n{HELP}");
            return ExitCode::from(2);
        }
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

/// Parse `--config`/`-c` and `--help`/`-h`. Returns the config path, or `None`
/// when help was printed, or an error message for bad usage.
fn parse_args() -> Result<Option<PathBuf>, String> {
    let mut config_path = PathBuf::from("config.toml");
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{HELP}");
                return Ok(None);
            }
            "-c" | "--config" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--config requires a path argument".to_owned())?;
                config_path = PathBuf::from(value);
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
    Ok(Some(config_path))
}

/// Build the app and serve until shutdown. Uses its own multi-thread runtime.
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
        let (auth_runtime, usage_logger, usage_writer, key_backfill) = if config.auth.enabled {
            let (runtime, logger, writer, backfill) =
                boot_auth_stack(&config, &mut provider_specs).await?;
            (Some(runtime), Some(logger), Some(writer), backfill)
        } else {
            (None, None, None, std::collections::HashMap::new())
        };

        // Connect timeout is client-wide (one pooled client); the overall cap
        // is a backstop above the executor's total timeout (M6 §6.4).
        let client = lumen_providers::http::build_client_with(
            Duration::from_millis(config.resilience.connect_timeout_ms),
            Duration::from_millis(config.resilience.total_timeout_ms.saturating_add(30_000)),
        );
        let registry = Arc::new(
            lumen_providers::Registry::build(provider_specs, client.clone())
                .context("failed to build provider registry")?,
        );

        // The price table lives in a shared cell so the hot reloader swaps the
        // very cell the handlers read (DEBT-1).
        let pricing = Arc::new(ArcSwap::from_pointee(CostTable::from_config(&config)));

        // Config hot reload (M7 §7.3): SIGHUP or a config-file change re-validates
        // and atomically swaps the routing table, price table and resilience
        // policy (circuit-breaker state preserved). A watcher-setup failure only
        // disables reload - the server still runs - so it is logged, not fatal.
        let reload_targets = ReloadTargets {
            registry: Arc::clone(&registry),
            pricing: Arc::clone(&pricing),
            resilience: Arc::clone(&resilience),
            metrics: reload_metrics,
            key_backfill,
        };
        match spawn_config_reloader(config_path, reload_targets) {
            Ok(_handle) => tracing::info!("config hot reload armed (SIGHUP + file watch)"),
            Err(error) => tracing::warn!(%error, "config hot reload unavailable"),
        }

        let health = boot_health(&config, &client, &resilience_metrics);

        let mut state = AppState::new(metrics, registry, tokens)
            .with_guards(guards)
            .with_pricing_cell(pricing)
            .with_resilience(resilience)
            .with_health(health)
            .with_body_limit(config.server.body_limit);
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

        // Final budget flush so a clean shutdown loses zero accounting.
        if let Some(runtime) = auth_runtime {
            let dirty = runtime.keys.drain_dirty();
            if !dirty.is_empty() {
                if let Err(error) = runtime.store.persist_budgets(&dirty).await {
                    tracing::warn!(%error, "final budget flush failed");
                }
            }
        }

        // Drain the usage writer: `serve` returning dropped the app (and with
        // it every UsageLogger clone), which closes the channel; the writer
        // then flushes what is buffered and exits. Bounded wait - shutdown
        // must never hang on a sick database.
        if let Some(writer) = usage_writer {
            if tokio::time::timeout(Duration::from_secs(5), writer)
                .await
                .is_err()
            {
                tracing::warn!("usage writer did not drain within 5s; giving up");
            }
        }

        tracing::info!("shutdown complete");
        Ok(())
    })
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

/// Boot the M5 auth stack: master key, SQLite store, provider-key back-fill,
/// in-memory key table, usage writer, periodic budget flush and retention
/// purge. Returns the runtime and the usage-log handle.
async fn boot_auth_stack(
    config: &Config,
    provider_specs: &mut [lumen_providers::ProviderSpec],
) -> anyhow::Result<(
    Arc<AuthRuntime>,
    lumen_auth::usage::UsageLogger,
    tokio::task::JoinHandle<()>,
    std::collections::HashMap<String, String>,
)> {
    use zeroize::Zeroize;
    let mut master_value = std::env::var(MASTER_KEY_ENV).with_context(|| {
        format!("auth.enabled requires the {MASTER_KEY_ENV} env var (64 hex chars)")
    })?;
    let master = MasterKey::from_env_value(&master_value)
        .with_context(|| format!("invalid {MASTER_KEY_ENV}"))?;

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
    // last flush at boot.
    let flush_runtime = Arc::clone(&runtime);
    let flush_interval = Duration::from_millis(config.auth.flush_interval_ms);
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(flush_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let dirty = flush_runtime.keys.drain_dirty();
            if dirty.is_empty() {
                continue;
            }
            if let Err(error) = flush_runtime.store.persist_budgets(&dirty).await {
                tracing::warn!(%error, "budget flush failed; will retry next interval");
            }
        }
    });

    // Retention purge: drop usage_log rows older than the window.
    let retention = i64::from(config.auth.retention_days) * 86_400;
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(3_600));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            match store.purge_usage_older_than(now_unix() - retention).await {
                Ok(0) => {}
                Ok(purged) => tracing::info!(purged, "usage-log retention purge"),
                Err(error) => tracing::warn!(%error, "usage-log purge failed"),
            }
        }
    });

    Ok((runtime, logger, writer, key_backfill))
}
