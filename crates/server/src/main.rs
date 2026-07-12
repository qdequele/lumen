//! Ferrogate server entry point.
//!
//! Thin orchestration only: parse args, load config, initialise logging, then
//! hand off to the library. `anyhow` is used here (and only here).

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::Context;
use ferrogate_server::{
    build_app, build_registry, config::Config, lifecycle, log_startup, state::AppState,
};
use ferrogate_telemetry::{logging::init_logging, Metrics};
use tokio::net::TcpListener;

/// How long to drain in-flight requests after a shutdown signal.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

const HELP: &str = "\
ferrogate — universal LLM gateway

USAGE:
    ferrogate [--config <PATH>]

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

    match run(config) {
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
fn run(config: Config) -> anyhow::Result<()> {
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

        let registry = build_registry(&config).context("failed to build provider registry")?;
        let guards = ferrogate_server::StreamGuards {
            first_token_timeout: Duration::from_millis(config.server.first_token_timeout_ms),
            heartbeat_interval: Duration::from_millis(config.server.sse_heartbeat_ms),
        };
        let state = AppState::new(Metrics::new(), registry).with_guards(guards);
        let app = build_app(state, config.server.body_limit);

        lifecycle::serve(listener, app, DRAIN_TIMEOUT, lifecycle::shutdown_signal())
            .await
            .context("server error")?;

        tracing::info!("shutdown complete");
        Ok(())
    })
}
