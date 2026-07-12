//! Configuration loading and validation.
//!
//! Config comes from a TOML file, overlaid with `FERROGATE_*` environment
//! variables (nested keys use `__`, e.g. `FERROGATE_SERVER__PORT=9090`).
//!
//! # Secrets
//! API keys are NEVER stored in this config — only the *name* of the
//! environment variable that holds each key (`api_key_env = "OPENAI_API_KEY"`).
//! The actual secret is read from the environment at provider-construction
//! time, so deriving `Debug` on these structs cannot leak a key.

use ferrogate_core::Capability;
use ferrogate_telemetry::logging::LogFormat;
use figment::{
    providers::{Env, Format, Toml},
    Figment,
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::Path;

/// Top-level gateway configuration.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// HTTP server settings.
    #[serde(default)]
    pub server: ServerConfig,
    /// Configured upstream providers.
    #[serde(default)]
    pub providers: Vec<ProviderConfig>,
    /// Log output format.
    #[serde(default)]
    pub log_format: LogFormatConfig,
}

/// HTTP server settings.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Bind address. Defaults to `127.0.0.1`.
    #[serde(default = "default_host")]
    pub host: String,
    /// Bind port. Defaults to `8080`.
    #[serde(default = "default_port")]
    pub port: u16,
    /// Maximum request body size in bytes. Defaults to 10 MiB.
    #[serde(default = "default_body_limit")]
    pub body_limit: usize,
}

/// A single upstream provider and the models it serves.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    /// Unique, user-chosen name for this provider instance.
    pub name: String,
    /// Which built-in provider implementation backs it.
    pub kind: ProviderKind,
    /// Name of the env var holding the API key (never the key itself).
    /// Optional for keyless local providers (Ollama, TEI).
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Override the provider's default base URL (required for self-hosted).
    #[serde(default)]
    pub base_url: Option<String>,
    /// Models this provider exposes.
    #[serde(default)]
    pub models: Vec<ModelConfig>,
}

/// A model exposed by the gateway, mapped to an upstream model id.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelConfig {
    /// The model id clients use (owned entirely by the operator).
    pub id: String,
    /// The upstream model id to send. Defaults to `id` when omitted.
    #[serde(default)]
    pub upstream_id: Option<String>,
    /// Capabilities this model serves.
    pub capabilities: Vec<Capability>,
}

impl ModelConfig {
    /// The upstream model id to send to the provider (falls back to `id`).
    #[must_use]
    pub fn resolved_upstream_id(&self) -> &str {
        self.upstream_id.as_deref().unwrap_or(&self.id)
    }
}

/// The built-in provider implementations Ferrogate knows how to talk to.
///
/// An unknown `kind` in the TOML is a hard error at load time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Openai,
    Anthropic,
    Cohere,
    Ollama,
    Tei,
    Jina,
    Mistral,
    Google,
}

/// Log output format, mirrored to [`LogFormat`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LogFormatConfig {
    /// Human-readable output (default for local dev).
    #[default]
    Pretty,
    /// JSON lines (recommended in production).
    Json,
}

impl From<LogFormatConfig> for LogFormat {
    fn from(value: LogFormatConfig) -> Self {
        match value {
            LogFormatConfig::Pretty => LogFormat::Pretty,
            LogFormatConfig::Json => LogFormat::Json,
        }
    }
}

fn default_host() -> String {
    "127.0.0.1".to_owned()
}
const fn default_port() -> u16 {
    8080
}
const fn default_body_limit() -> usize {
    10 * 1024 * 1024
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            body_limit: default_body_limit(),
        }
    }
}

/// A description of one loaded model, safe to log (no secrets).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedModel {
    /// Client-facing model id.
    pub id: String,
    /// The provider serving it.
    pub provider: String,
    /// Capabilities it exposes.
    pub capabilities: Vec<Capability>,
}

/// Errors produced while loading or validating configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The explicitly-requested config file does not exist.
    #[error("config file '{path}' not found")]
    NotFound {
        /// The path that was requested.
        path: String,
    },
    /// The file could not be read or parsed, or a value had the wrong type.
    #[error("invalid config file '{path}': {message}")]
    Parse {
        /// The config file path (for the operator's benefit).
        path: String,
        /// Human-readable reason, naming the offending field where possible.
        message: String,
    },
    /// The config parsed but failed a semantic validation rule.
    #[error("invalid config '{path}': {message}")]
    Validation {
        /// The config file path.
        path: String,
        /// What is wrong, naming the field.
        message: String,
    },
}

impl Config {
    /// Load and validate configuration from `path`, overlaid with `FERROGATE_*`
    /// environment variables.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let label = path.display().to_string();
        // figment silently treats a missing TOML file as empty. Since the
        // operator explicitly requested this path, a missing file is an error,
        // not a fall-through to defaults.
        if !path.exists() {
            return Err(ConfigError::NotFound { path: label });
        }
        let figment = Figment::new()
            .merge(Toml::file(path))
            .merge(Env::prefixed("FERROGATE_").split("__"));
        Self::from_figment(&figment, &label)
    }

    /// Build a config from an arbitrary figment (used by tests) and validate it.
    fn from_figment(figment: &Figment, path_label: &str) -> Result<Self, ConfigError> {
        let config: Config = figment.extract().map_err(|e| ConfigError::Parse {
            path: path_label.to_owned(),
            message: e.to_string(),
        })?;
        config.validate(path_label)?;
        Ok(config)
    }

    /// Semantic validation, beyond what the type system and serde enforce.
    fn validate(&self, path_label: &str) -> Result<(), ConfigError> {
        let err = |message: String| ConfigError::Validation {
            path: path_label.to_owned(),
            message,
        };

        if self.server.port == 0 {
            return Err(err("server.port must not be 0".to_owned()));
        }

        let mut provider_names = HashSet::new();
        let mut model_ids = HashSet::new();
        for provider in &self.providers {
            if provider.name.trim().is_empty() {
                return Err(err("a provider has an empty 'name'".to_owned()));
            }
            if !provider_names.insert(provider.name.as_str()) {
                return Err(err(format!("duplicate provider name '{}'", provider.name)));
            }
            for model in &provider.models {
                if model.id.trim().is_empty() {
                    return Err(err(format!(
                        "provider '{}' has a model with an empty 'id'",
                        provider.name
                    )));
                }
                if model.capabilities.is_empty() {
                    return Err(err(format!(
                        "model '{}' must declare at least one capability",
                        model.id
                    )));
                }
                if !model_ids.insert(model.id.as_str()) {
                    return Err(err(format!(
                        "duplicate model id '{}' (model ids must be unique across providers)",
                        model.id
                    )));
                }
            }
        }
        Ok(())
    }

    /// A secret-free summary of every loaded model, for the boot log.
    #[must_use]
    pub fn loaded_models(&self) -> Vec<LoadedModel> {
        self.providers
            .iter()
            .flat_map(|p| {
                p.models.iter().map(move |m| LoadedModel {
                    id: m.id.clone(),
                    provider: p.name.clone(),
                    capabilities: m.capabilities.clone(),
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"
        [server]
        host = "0.0.0.0"
        port = 8080

        [[providers]]
        name = "openai-main"
        kind = "openai"
        api_key_env = "OPENAI_API_KEY"

        [[providers.models]]
        id = "gpt-4o"
        upstream_id = "gpt-4o-2024-08-06"
        capabilities = ["chat"]

        [[providers.models]]
        id = "text-embed"
        capabilities = ["embed"]
    "#;

    fn load_str(s: &str) -> Result<Config, ConfigError> {
        let figment = Figment::new().merge(Toml::string(s));
        Config::from_figment(&figment, "test.toml")
    }

    #[test]
    fn valid_config_parses_and_resolves_defaults() {
        let cfg = load_str(VALID).unwrap();
        assert_eq!(cfg.server.port, 8080);
        assert_eq!(cfg.server.body_limit, 10 * 1024 * 1024); // default applied
        assert_eq!(cfg.providers.len(), 1);
        let models = &cfg.providers[0].models;
        assert_eq!(models[0].resolved_upstream_id(), "gpt-4o-2024-08-06");
        // upstream_id defaults to id when omitted
        assert_eq!(models[1].resolved_upstream_id(), "text-embed");
    }

    #[test]
    fn empty_config_uses_all_defaults() {
        let cfg = load_str("").unwrap();
        assert_eq!(cfg.server.host, "127.0.0.1");
        assert_eq!(cfg.server.port, 8080);
        assert!(cfg.providers.is_empty());
    }

    #[test]
    fn unknown_field_is_rejected_and_named() {
        let err = load_str("[server]\nportt = 9090\n").unwrap_err();
        let msg = err.to_string();
        // figment/serde names the unknown key.
        assert!(
            msg.contains("portt"),
            "message should name the field: {msg}"
        );
    }

    #[test]
    fn invalid_port_zero_is_rejected_and_named() {
        let err = load_str("[server]\nport = 0\n").unwrap_err();
        assert!(matches!(err, ConfigError::Validation { .. }));
        assert!(err.to_string().contains("port"));
    }

    #[test]
    fn out_of_range_port_is_rejected() {
        let err = load_str("[server]\nport = 99999\n").unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }));
    }

    #[test]
    fn unknown_provider_kind_is_rejected() {
        let toml = "[[providers]]\nname = \"x\"\nkind = \"not_a_provider\"\n";
        let err = load_str(toml).unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }));
    }

    #[test]
    fn duplicate_model_id_is_rejected() {
        let toml = r#"
            [[providers]]
            name = "a"
            kind = "openai"
            [[providers.models]]
            id = "dup"
            capabilities = ["chat"]
            [[providers.models]]
            id = "dup"
            capabilities = ["embed"]
        "#;
        let err = load_str(toml).unwrap_err();
        assert!(err.to_string().contains("dup"));
    }

    #[test]
    fn model_without_capability_is_rejected() {
        let toml = r#"
            [[providers]]
            name = "a"
            kind = "openai"
            [[providers.models]]
            id = "m"
            capabilities = []
        "#;
        let err = load_str(toml).unwrap_err();
        assert!(err.to_string().contains("capability"));
    }

    #[test]
    fn missing_config_file_is_an_error_not_silent_defaults() {
        let err = Config::load(Path::new("/tmp/ferrogate-does-not-exist-xyz.toml")).unwrap_err();
        assert!(matches!(err, ConfigError::NotFound { .. }));
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn env_var_overrides_file_value() {
        // The closure must return `figment::Error`, whose size we don't control.
        #[allow(clippy::result_large_err)]
        figment::Jail::expect_with(|jail| {
            jail.create_file("config.toml", VALID)?;
            jail.set_env("FERROGATE_SERVER__PORT", "9090");
            let cfg = Config::load(Path::new("config.toml")).unwrap();
            assert_eq!(cfg.server.port, 9090);
            Ok(())
        });
    }

    #[test]
    fn config_never_holds_a_secret_only_env_var_names() {
        let cfg = load_str(VALID).unwrap();
        // The config references the key by env var NAME, never a value.
        assert_eq!(
            cfg.providers[0].api_key_env.as_deref(),
            Some("OPENAI_API_KEY")
        );
        // Debug output contains the var name but no key material could exist.
        let dbg = format!("{cfg:?}");
        assert!(dbg.contains("OPENAI_API_KEY"));
    }

    #[test]
    fn loaded_models_summary_lists_all_models() {
        let cfg = load_str(VALID).unwrap();
        let models = cfg.loaded_models();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].provider, "openai-main");
        assert!(models.iter().any(|m| m.id == "gpt-4o"));
    }
}
