//! LUMEN server library: config, HTTP app assembly and lifecycle.
//!
//! The binary in `main.rs` is a thin wrapper around this crate so the app can
//! be driven directly from integration tests.

#![forbid(unsafe_code)]

pub mod accounting;
pub mod admin;
pub mod app;
pub mod auth;
pub mod chat;
pub mod config;
pub mod embeddings;
pub mod error;
pub mod health;
pub mod lifecycle;
pub mod metadata;
pub mod models;
pub mod pricing;
pub mod reload;
pub mod rerank;
pub mod resilience;
pub mod routes;
pub mod state;
pub mod tokenizer;

pub use app::build_app;
pub use config::{Config, ConfigError};
pub use lifecycle::{serve, shutdown_signal};
pub use state::{AppState, StreamGuards};

use lumen_providers::Registry;
use std::path::Path;
use std::sync::Arc;

/// Build the provider registry from config, sharing one HTTP client.
///
/// # Errors
/// Returns a [`RegistryError`](lumen_providers::RegistryError) if a provider
/// spec is invalid (e.g. a keyless provider missing its required `base_url`).
pub fn build_registry(config: &Config) -> Result<Arc<Registry>, lumen_providers::RegistryError> {
    let client = lumen_providers::http::build_client();
    // `build_client` uses the 300 s default overall cap; a per-provider connect
    // override reuses that same backstop (ADR 005, 2026-07-15 amendment).
    let registry = Registry::build(
        config.provider_specs(),
        client,
        std::time::Duration::from_secs(300),
    )?;
    Ok(Arc::new(registry))
}

/// A secret-free summary of a config that passed `check_config`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigCheckReport {
    /// Number of providers declared.
    pub provider_count: usize,
    /// Number of models declared across all providers.
    pub model_count: usize,
}

/// Failure while validating a config file for `--check-config`.
#[derive(Debug, thiserror::Error)]
pub enum ConfigCheckError {
    /// The config file could not be found, parsed, or failed its own
    /// semantic validation (see [`ConfigError`]).
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// The config parsed but describes an invalid provider registry (e.g. a
    /// self-hosted provider missing its required `base_url`).
    #[error(transparent)]
    Registry(#[from] lumen_providers::RegistryError),
}

/// Load and fully validate the config at `path` the same way the server does
/// at boot: parsing, semantic validation (`Config::load`) and provider
/// registry construction (`build_registry`), which additionally catches
/// provider/model reference errors that only surface once the registry is
/// built (e.g. a missing `base_url`).
///
/// Deliberately local-only, so it is safe to run ahead of a real boot in a CI
/// or deploy pipeline: it never binds a listener, opens the auth database, or
/// contacts a provider.
///
/// # Errors
/// Returns [`ConfigCheckError`] if the config fails to load, fails semantic
/// validation, or describes an invalid provider registry.
pub fn check_config(path: &Path) -> Result<ConfigCheckReport, ConfigCheckError> {
    let config = Config::load(path)?;
    let provider_count = config.providers.len();
    let model_count = config.providers.iter().map(|p| p.models.len()).sum();
    build_registry(&config)?;
    Ok(ConfigCheckReport {
        provider_count,
        model_count,
    })
}

/// Emit the startup log line and one line per loaded model.
///
/// Only secret-free metadata is logged (model id, provider name, capabilities);
/// API keys are never touched here - the config only holds env var *names*.
pub fn log_startup(config: &Config) {
    let models = config.loaded_models();
    tracing::info!(
        model_count = models.len(),
        provider_count = config.providers.len(),
        "lumen starting"
    );
    for model in &models {
        let capabilities: Vec<&str> = model.capabilities.iter().map(|c| c.as_str()).collect();
        tracing::info!(
            model = %model.id,
            provider = %model.provider,
            ?capabilities,
            "loaded model"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use figment::{
        providers::{Format, Toml},
        Figment,
    };
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    /// A `MakeWriter` that appends everything into a shared buffer, so a test
    /// can inspect exactly what was logged.
    #[derive(Clone)]
    struct BufMakeWriter(Arc<Mutex<Vec<u8>>>);

    struct BufGuard(Arc<Mutex<Vec<u8>>>);

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for BufMakeWriter {
        type Writer = BufGuard;
        fn make_writer(&'a self) -> Self::Writer {
            BufGuard(self.0.clone())
        }
    }

    impl Write for BufGuard {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("log buffer poisoned")
                .extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn config_with_key_env() -> Config {
        // Loaded via the crate's own loader so this exercises the real path.
        // NOTE: the env var name deliberately has no `LUMEN_` prefix, so it
        // is never picked up as a config override by the figment `Env` source.
        let toml = r#"
            [[providers]]
            name = "openai-main"
            kind = "openai"
            api_key_env = "BOOT_LOG_TEST_SECRET"
            [[providers.models]]
            id = "gpt-4o"
            capabilities = ["chat"]
        "#;
        let figment = Figment::new().merge(Toml::string(toml));
        // Reuse the private loader via the public `load`-equivalent by writing
        // to a jail would be heavier; parse directly through the public API.
        figment.extract::<Config>().expect("valid test config")
    }

    #[test]
    fn boot_log_lists_models_but_never_logs_api_key_values() {
        // A real secret sitting in the referenced env var must never be logged.
        // (Safe on edition 2021; the value is fake and scoped to this test.)
        std::env::set_var("BOOT_LOG_TEST_SECRET", "sk-DO-NOT-LOG-THIS-VALUE");

        let config = config_with_key_env();

        let buffer = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(BufMakeWriter(buffer.clone()))
            .with_ansi(false)
            .finish();

        tracing::subscriber::with_default(subscriber, || log_startup(&config));

        let logs = String::from_utf8(buffer.lock().unwrap().clone()).unwrap();

        // The model is logged...
        assert!(logs.contains("gpt-4o"), "expected model id in logs: {logs}");
        assert!(logs.contains("openai-main"));
        // ...but neither the secret value nor even the raw env-var name's value.
        assert!(
            !logs.contains("sk-DO-NOT-LOG-THIS-VALUE"),
            "API key value leaked into logs: {logs}"
        );

        std::env::remove_var("BOOT_LOG_TEST_SECRET");
    }

    /// Write `toml` to a fresh temp file and return the guard (dropping it
    /// deletes the file). `check_config` needs a real path on disk - unlike
    /// `Config`'s own tests, it cannot go through an in-memory figment
    /// source.
    fn write_temp_config(toml: &str) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().expect("create temp config file");
        file.write_all(toml.as_bytes())
            .expect("write temp config file");
        file
    }

    #[test]
    fn check_config_accepts_a_valid_config_and_reports_counts() {
        let file = write_temp_config(
            r#"
            [[providers]]
            name = "openai-main"
            kind = "openai"

            [[providers.models]]
            id = "gpt-4o"
            capabilities = ["chat"]

            [[providers.models]]
            id = "text-embed"
            capabilities = ["embed"]
            "#,
        );

        let report = check_config(file.path()).expect("valid config should pass");
        assert_eq!(report.provider_count, 1);
        assert_eq!(report.model_count, 2);
    }

    #[test]
    fn check_config_rejects_a_missing_file() {
        let missing = std::path::Path::new("/tmp/lumen-check-config-does-not-exist.toml");
        let err = check_config(missing).expect_err("missing file must be rejected");
        assert!(matches!(
            err,
            ConfigCheckError::Config(ConfigError::NotFound { .. })
        ));
    }

    #[test]
    fn check_config_rejects_a_semantically_invalid_config() {
        // server.port = 0 fails Config's own semantic validation.
        let file = write_temp_config("[server]\nport = 0\n");
        let err = check_config(file.path()).expect_err("port 0 must be rejected");
        assert!(matches!(
            err,
            ConfigCheckError::Config(ConfigError::Validation { .. })
        ));
    }

    #[test]
    fn check_config_rejects_a_provider_missing_its_required_base_url() {
        // The registry (not Config::validate) is where a missing base_url on
        // a kind with no vendor default (e.g. vllm) is caught - so
        // check_config must build the registry, not just load the config.
        let file = write_temp_config(
            r#"
            [[providers]]
            name = "self-hosted"
            kind = "vllm"

            [[providers.models]]
            id = "local-model"
            capabilities = ["chat"]
            "#,
        );

        let err = check_config(file.path()).expect_err("missing base_url must be rejected");
        assert!(matches!(
            err,
            ConfigCheckError::Registry(lumen_providers::RegistryError::MissingBaseUrl { .. })
        ));
    }
}
