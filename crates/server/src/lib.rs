//! Ferrogate server library: config, HTTP app assembly and lifecycle.
//!
//! The binary in `main.rs` is a thin wrapper around this crate so the app can
//! be driven directly from integration tests.

#![forbid(unsafe_code)]

pub mod app;
pub mod config;
pub mod lifecycle;
pub mod routes;
pub mod state;

pub use app::build_app;
pub use config::{Config, ConfigError};
pub use lifecycle::{serve, shutdown_signal};
pub use state::AppState;

/// Emit the startup log line and one line per loaded model.
///
/// Only secret-free metadata is logged (model id, provider name, capabilities);
/// API keys are never touched here — the config only holds env var *names*.
pub fn log_startup(config: &Config) {
    let models = config.loaded_models();
    tracing::info!(
        model_count = models.len(),
        provider_count = config.providers.len(),
        "ferrogate starting"
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
        // NOTE: the env var name deliberately has no `FERROGATE_` prefix, so it
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
}
