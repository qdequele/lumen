//! Configuration loading and validation.
//!
//! Config comes from a TOML file, overlaid with `LUMEN_*` environment
//! variables (nested keys use `__`, e.g. `LUMEN_SERVER__PORT=9090`).
//!
//! # Secrets
//! API keys are NEVER stored in this config - only the *name* of the
//! environment variable that holds each key (`api_key_env = "OPENAI_API_KEY"`).
//! The actual secret is read from the environment at provider-construction
//! time, so deriving `Debug` on these structs cannot leak a key.

use figment::{
    providers::{Env, Format, Toml},
    Figment,
};
use lumen_core::Capability;
use lumen_providers::{ModelSpec, ProviderKind, ProviderSpec};
use lumen_telemetry::logging::LogFormat;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// Top-level gateway configuration.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
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
    /// Virtual keys, budgets and usage logging (M5). Disabled by default.
    #[serde(default)]
    pub auth: AuthConfig,
    /// Telemetry knobs (metadata label allowlist, ADR 002).
    #[serde(default)]
    pub telemetry: TelemetryConfig,
    /// Resilience knobs: retries, circuit breaker, timeouts, health checks (M6).
    #[serde(default)]
    pub resilience: ResilienceConfig,
    /// Guarded server-side image fetching for multimodal embeddings (M9).
    #[serde(default)]
    pub image_fetch: ImageFetchConfig,
}

/// Retries, circuit breaker, timeouts and background health checks (M6).
///
/// `first_token` is not here - it stays [`ServerConfig::first_token_timeout_ms`]
/// (its M4 home) and can be overridden per provider. `connect` is a client-wide
/// setting (one pooled HTTP client), so it has no per-provider override.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResilienceConfig {
    /// Total attempts per provider, including the first (`1` disables retries).
    #[serde(default = "default_retry_max_attempts")]
    pub retry_max_attempts: u32,
    /// Base backoff delay in ms (pre-jitter wait after the first failure).
    #[serde(default = "default_retry_base_ms")]
    pub retry_base_ms: u64,
    /// Ceiling on the exponential backoff term in ms.
    #[serde(default = "default_retry_max_ms")]
    pub retry_max_ms: u64,
    /// Consecutive provider-fault failures that trip a circuit open.
    #[serde(default = "default_circuit_failure_threshold")]
    pub circuit_failure_threshold: u32,
    /// How long a circuit stays open before a half-open probe, in ms.
    #[serde(default = "default_circuit_cooldown_ms")]
    pub circuit_cooldown_ms: u64,
    /// Connection-establishment timeout in ms (client-wide).
    #[serde(default = "default_connect_timeout_ms")]
    pub connect_timeout_ms: u64,
    /// Overall per-request timeout in ms (all retries + fallbacks together).
    #[serde(default = "default_total_timeout_ms")]
    pub total_timeout_ms: u64,
    /// Enable the background provider health-check probe (default off).
    #[serde(default)]
    pub health_check_enabled: bool,
    /// How often the health-check probe runs, in ms.
    #[serde(default = "default_health_check_interval_ms")]
    pub health_check_interval_ms: u64,
}

impl Default for ResilienceConfig {
    fn default() -> Self {
        Self {
            retry_max_attempts: default_retry_max_attempts(),
            retry_base_ms: default_retry_base_ms(),
            retry_max_ms: default_retry_max_ms(),
            circuit_failure_threshold: default_circuit_failure_threshold(),
            circuit_cooldown_ms: default_circuit_cooldown_ms(),
            connect_timeout_ms: default_connect_timeout_ms(),
            total_timeout_ms: default_total_timeout_ms(),
            health_check_enabled: false,
            health_check_interval_ms: default_health_check_interval_ms(),
        }
    }
}

impl ResilienceConfig {
    /// Validate the knobs: everything that must be non-zero (M6 §6.1/6.3/6.4).
    fn validate(&self, path_label: &str) -> Result<(), ConfigError> {
        let err = |message: String| ConfigError::Validation {
            path: path_label.to_owned(),
            message,
        };
        let checks: [(&str, u64); 7] = [
            (
                "resilience.retry_max_attempts",
                u64::from(self.retry_max_attempts),
            ),
            ("resilience.retry_base_ms", self.retry_base_ms),
            ("resilience.retry_max_ms", self.retry_max_ms),
            (
                "resilience.circuit_failure_threshold",
                u64::from(self.circuit_failure_threshold),
            ),
            ("resilience.circuit_cooldown_ms", self.circuit_cooldown_ms),
            ("resilience.connect_timeout_ms", self.connect_timeout_ms),
            ("resilience.total_timeout_ms", self.total_timeout_ms),
        ];
        for (field, value) in checks {
            if value == 0 {
                return Err(err(format!("{field} must not be 0")));
            }
        }
        if self.health_check_enabled && self.health_check_interval_ms == 0 {
            return Err(err(
                "resilience.health_check_interval_ms must not be 0 when health checks are enabled"
                    .to_owned(),
            ));
        }
        Ok(())
    }
}

/// Virtual-key auth, hard budgets and usage logging (M5).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    /// Master switch. When `false` (default) the gateway is open: no key
    /// checks, no budgets, no usage database.
    #[serde(default)]
    pub enabled: bool,
    /// SQLite database path (created if missing).
    #[serde(default = "default_db_path")]
    pub db_path: String,
    /// How often in-memory budget counters are flushed to the DB, in
    /// milliseconds. A crash loses at most this much *accounting*; it can
    /// never allow a budget overrun (enforcement is in memory).
    #[serde(default = "default_flush_interval_ms")]
    pub flush_interval_ms: u64,
    /// Bounded usage-log channel capacity.
    #[serde(default = "default_usage_channel_capacity")]
    pub usage_channel_capacity: usize,
    /// Usage-log batch size that triggers an immediate write.
    #[serde(default = "default_usage_batch_max")]
    pub usage_batch_max: usize,
    /// Maximum time a pending usage batch waits before being written, ms.
    #[serde(default = "default_usage_flush_ms")]
    pub usage_flush_ms: u64,
    /// Usage-log retention in days (purged by a background task).
    #[serde(default = "default_retention_days")]
    pub retention_days: u32,
}

impl AuthConfig {
    /// The sqlx connection URL for [`db_path`](Self::db_path).
    #[must_use]
    pub fn db_url(&self) -> String {
        format!("sqlite://{}", self.db_path)
    }
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            db_path: default_db_path(),
            flush_interval_ms: default_flush_interval_ms(),
            usage_channel_capacity: default_usage_channel_capacity(),
            usage_batch_max: default_usage_batch_max(),
            usage_flush_ms: default_usage_flush_ms(),
            retention_days: default_retention_days(),
        }
    }
}

/// Telemetry configuration (ADR 002).
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TelemetryConfig {
    /// Metadata keys allowed to become Prometheus labels. Default empty:
    /// client metadata NEVER creates time series unless the operator opts a
    /// key in here (cardinality stays operator-bounded).
    #[serde(default)]
    pub metadata_labels: Vec<String>,
}

/// Base label names already used by the token counters - an allowlisted
/// metadata key may not shadow them.
const RESERVED_LABELS: [&str; 5] = ["capability", "model", "provider", "direction", "estimated"];

impl TelemetryConfig {
    /// ADR 002: the allowlist is the ONLY thing that turns metadata into
    /// metric labels, so it must produce valid, non-colliding label names.
    fn validate(&self, path_label: &str) -> Result<(), ConfigError> {
        let err = |message: String| ConfigError::Validation {
            path: path_label.to_owned(),
            message,
        };
        if self.metadata_labels.len() > 16 {
            return Err(err(
                "telemetry.metadata_labels: at most 16 entries".to_owned()
            ));
        }
        let mut seen_labels = HashSet::new();
        for label in &self.metadata_labels {
            if !is_valid_label_name(label) {
                return Err(err(format!(
                    "telemetry.metadata_labels: '{label}' is not a valid Prometheus label \
                     name ([a-zA-Z_][a-zA-Z0-9_]*)"
                )));
            }
            if RESERVED_LABELS.contains(&label.as_str()) {
                return Err(err(format!(
                    "telemetry.metadata_labels: '{label}' collides with a built-in label"
                )));
            }
            if !seen_labels.insert(label.as_str()) {
                return Err(err(format!(
                    "telemetry.metadata_labels: duplicate entry '{label}'"
                )));
            }
        }
        Ok(())
    }
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
    /// How long to wait for the upstream's first sign of life before failing
    /// with LM-3011 (504), in milliseconds. Streaming: time to the first SSE
    /// frame; non-streaming: the whole upstream call. Defaults to 30 000.
    #[serde(default = "default_first_token_timeout_ms")]
    pub first_token_timeout_ms: u64,
    /// Idle interval after which a `: ping` SSE comment is sent on silent
    /// streams (keep-alive for proxies), in milliseconds. Defaults to 15 000.
    #[serde(default = "default_sse_heartbeat_ms")]
    pub sse_heartbeat_ms: u64,
}

/// Guarded server-side image fetching for multimodal embeddings (M9).
///
/// Off by default. When enabled, remote `http(s)` image URLs in an embeddings
/// request are fetched under SSRF/resource guards and inlined as `data:` URIs.
/// The private-IP block is always on and has no config knob.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ImageFetchConfig {
    /// Master switch. `false` → a remote image URL is rejected with `LM-2005`.
    #[serde(default)]
    pub enabled: bool,
    /// Maximum bytes downloaded per image.
    #[serde(default = "default_image_max_bytes")]
    pub max_bytes: u64,
    /// Per-fetch timeout in milliseconds.
    #[serde(default = "default_image_timeout_ms")]
    pub timeout_ms: u64,
    /// Permitted URL schemes. Defaults to `["https"]`.
    #[serde(default = "default_image_schemes")]
    pub allowed_schemes: Vec<String>,
    /// Permitted hosts (exact, or `.suffix` for a domain + subdomains). Empty =
    /// any public host.
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
    /// Permitted URL prefixes. Empty = no prefix restriction.
    #[serde(default)]
    pub allowed_url_prefixes: Vec<String>,
}

impl Default for ImageFetchConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_bytes: default_image_max_bytes(),
            timeout_ms: default_image_timeout_ms(),
            allowed_schemes: default_image_schemes(),
            allowed_hosts: Vec::new(),
            allowed_url_prefixes: Vec::new(),
        }
    }
}

impl ImageFetchConfig {
    /// Build the runtime policy. `allow_private_ips` is hard-wired to `false`:
    /// the private-IP SSRF block is never configurable.
    #[must_use]
    pub fn to_policy(&self) -> lumen_providers::image_fetch::ImageFetchPolicy {
        lumen_providers::image_fetch::ImageFetchPolicy {
            enabled: self.enabled,
            max_bytes: self.max_bytes,
            timeout: std::time::Duration::from_millis(self.timeout_ms),
            allowed_schemes: self.allowed_schemes.clone(),
            allowed_hosts: self.allowed_hosts.clone(),
            allowed_url_prefixes: self.allowed_url_prefixes.clone(),
            allow_private_ips: false,
        }
    }

    /// Whether fetching is enabled with no host/prefix allowlist - worth a
    /// startup warning (only the scheme and private-IP guards then apply).
    #[must_use]
    pub fn is_unrestricted(&self) -> bool {
        self.enabled && self.allowed_hosts.is_empty() && self.allowed_url_prefixes.is_empty()
    }
}

/// A single upstream provider and the models it serves.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
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
    /// Per-provider first-token timeout override in ms (else the global
    /// [`ServerConfig::first_token_timeout_ms`]).
    #[serde(default)]
    pub first_token_timeout_ms: Option<u64>,
    /// Per-provider total timeout override in ms (else the global
    /// [`ResilienceConfig::total_timeout_ms`]).
    #[serde(default)]
    pub total_timeout_ms: Option<u64>,
    /// Models this provider exposes.
    #[serde(default)]
    pub models: Vec<ModelConfig>,
}

/// A model exposed by the gateway, mapped to an upstream model id.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelConfig {
    /// The model id clients use (owned entirely by the operator).
    pub id: String,
    /// The upstream model id to send. Defaults to `id` when omitted.
    #[serde(default)]
    pub upstream_id: Option<String>,
    /// Capabilities this model serves.
    pub capabilities: Vec<Capability>,
    /// Modalities this model accepts as input. Defaults to `["text"]`; add
    /// `"image"` to allow image content parts on chat (vision) and embeddings.
    /// Unknown modalities parse but are ignored in this release.
    #[serde(default = "default_modalities")]
    pub modalities: Vec<String>,
    /// Price per **million input tokens**, USD (M5 cost counting).
    #[serde(default)]
    pub cost_per_1m_input: Option<f64>,
    /// Price per **million output tokens**, USD.
    #[serde(default)]
    pub cost_per_1m_output: Option<f64>,
    /// Price per **thousand rerank searches**, USD.
    #[serde(default)]
    pub cost_per_1k_searches: Option<f64>,
    /// Ordered fallback model ids tried, in turn, after this model's provider
    /// exhausts its retries or its circuit is open (M6 §6.2). Each must exist
    /// and serve every capability this model declares (validated at boot).
    #[serde(default)]
    pub fallbacks: Vec<String>,
}

impl ModelConfig {
    /// The upstream model id to send to the provider (falls back to `id`).
    #[must_use]
    pub fn resolved_upstream_id(&self) -> &str {
        self.upstream_id.as_deref().unwrap_or(&self.id)
    }
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

/// Prometheus label names: `[a-zA-Z_][a-zA-Z0-9_]*`.
fn is_valid_label_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
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
fn default_modalities() -> Vec<String> {
    vec!["text".to_owned()]
}
const fn default_image_max_bytes() -> u64 {
    10 * 1024 * 1024
}
const fn default_image_timeout_ms() -> u64 {
    5000
}
fn default_image_schemes() -> Vec<String> {
    vec!["https".to_owned()]
}
const fn default_first_token_timeout_ms() -> u64 {
    30_000
}
const fn default_sse_heartbeat_ms() -> u64 {
    15_000
}
fn default_db_path() -> String {
    "lumen.db".to_owned()
}
const fn default_flush_interval_ms() -> u64 {
    10_000
}
const fn default_usage_channel_capacity() -> usize {
    10_000
}
const fn default_usage_batch_max() -> usize {
    500
}
const fn default_usage_flush_ms() -> u64 {
    2_000
}
const fn default_retention_days() -> u32 {
    30
}
const fn default_retry_max_attempts() -> u32 {
    3
}
const fn default_retry_base_ms() -> u64 {
    200
}
const fn default_retry_max_ms() -> u64 {
    5_000
}
const fn default_circuit_failure_threshold() -> u32 {
    5
}
const fn default_circuit_cooldown_ms() -> u64 {
    30_000
}
const fn default_connect_timeout_ms() -> u64 {
    5_000
}
const fn default_total_timeout_ms() -> u64 {
    600_000
}
const fn default_health_check_interval_ms() -> u64 {
    30_000
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            body_limit: default_body_limit(),
            first_token_timeout_ms: default_first_token_timeout_ms(),
            sse_heartbeat_ms: default_sse_heartbeat_ms(),
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
    /// Load and validate configuration from `path`, overlaid with `LUMEN_*`
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
            .merge(Env::prefixed("LUMEN_").split("__"));
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
        if self.server.first_token_timeout_ms == 0 {
            return Err(err("server.first_token_timeout_ms must not be 0".to_owned()));
        }
        if self.server.sse_heartbeat_ms == 0 {
            return Err(err("server.sse_heartbeat_ms must not be 0".to_owned()));
        }

        if self.auth.enabled {
            if self.auth.db_path.trim().is_empty() {
                return Err(err("auth.db_path must not be empty".to_owned()));
            }
            if self.auth.flush_interval_ms == 0 {
                return Err(err("auth.flush_interval_ms must not be 0".to_owned()));
            }
            if self.auth.usage_channel_capacity == 0 {
                return Err(err("auth.usage_channel_capacity must not be 0".to_owned()));
            }
            if self.auth.usage_batch_max == 0 {
                return Err(err("auth.usage_batch_max must not be 0".to_owned()));
            }
            if self.auth.usage_flush_ms == 0 {
                return Err(err("auth.usage_flush_ms must not be 0".to_owned()));
            }
            if self.auth.retention_days == 0 {
                return Err(err("auth.retention_days must not be 0".to_owned()));
            }
        }

        self.telemetry.validate(path_label)?;
        self.resilience.validate(path_label)?;

        for provider in &self.providers {
            for (field, value) in [
                ("first_token_timeout_ms", provider.first_token_timeout_ms),
                ("total_timeout_ms", provider.total_timeout_ms),
            ] {
                if value == Some(0) {
                    return Err(err(format!(
                        "provider '{}': {field} must not be 0",
                        provider.name
                    )));
                }
            }
        }

        let mut provider_names = HashSet::new();
        // model id -> the provider that first declared it, so a collision can
        // cite BOTH conflicting locations (M3 acceptance criterion 4).
        let mut model_owner: HashMap<&str, &str> = HashMap::new();
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
                for (field, value) in [
                    ("cost_per_1m_input", model.cost_per_1m_input),
                    ("cost_per_1m_output", model.cost_per_1m_output),
                    ("cost_per_1k_searches", model.cost_per_1k_searches),
                ] {
                    if value.is_some_and(|v| !v.is_finite() || v < 0.0) {
                        return Err(err(format!(
                            "model '{}': {field} must be a finite, non-negative number",
                            model.id
                        )));
                    }
                }
                if let Some(first_owner) = model_owner.insert(model.id.as_str(), &provider.name) {
                    return Err(err(format!(
                        "duplicate model id '{}': declared by both provider '{}' and provider \
                         '{}' (model ids must be unique across providers; use distinct aliases \
                         and map each to its upstream_id)",
                        model.id, first_owner, provider.name
                    )));
                }
            }
        }

        self.validate_fallbacks(&err)?;
        Ok(())
    }

    /// Validate every model's fallback chain (M6 §6.2): each fallback id must
    /// exist, differ from the model itself, and serve every capability the
    /// model declares (so any request routed to the model can fall over to it).
    fn validate_fallbacks(&self, err: &impl Fn(String) -> ConfigError) -> Result<(), ConfigError> {
        // model id -> its declared capabilities, across all providers.
        let mut caps: HashMap<&str, &[Capability]> = HashMap::new();
        for provider in &self.providers {
            for model in &provider.models {
                caps.insert(model.id.as_str(), &model.capabilities);
            }
        }
        for provider in &self.providers {
            for model in &provider.models {
                for fallback in &model.fallbacks {
                    if fallback == &model.id {
                        return Err(err(format!(
                            "model '{}' lists itself as a fallback",
                            model.id
                        )));
                    }
                    let Some(fallback_caps) = caps.get(fallback.as_str()) else {
                        return Err(err(format!(
                            "model '{}' has an unknown fallback '{fallback}'",
                            model.id
                        )));
                    };
                    if let Some(missing) = model
                        .capabilities
                        .iter()
                        .find(|c| !fallback_caps.contains(c))
                    {
                        return Err(err(format!(
                            "fallback '{fallback}' for model '{}' does not serve capability \
                             '{missing}' (a fallback must serve every capability of the model \
                             it backs)",
                            model.id
                        )));
                    }
                }
            }
        }
        Ok(())
    }

    /// The ordered fallback chain for each model id (primary first), derived
    /// from `fallbacks`. Models without fallbacks are omitted.
    #[must_use]
    pub fn fallback_map(&self) -> HashMap<String, Vec<String>> {
        let mut map = HashMap::new();
        for provider in &self.providers {
            for model in &provider.models {
                if !model.fallbacks.is_empty() {
                    map.insert(model.id.clone(), model.fallbacks.clone());
                }
            }
        }
        map
    }

    /// Per-model timeout overrides (first-token, total) inherited from the
    /// owning provider. Models whose provider sets no override are omitted
    /// (the caller applies the global defaults).
    #[must_use]
    pub fn model_timeout_overrides(&self) -> HashMap<String, (Option<u64>, Option<u64>)> {
        let mut map = HashMap::new();
        for provider in &self.providers {
            if provider.first_token_timeout_ms.is_none() && provider.total_timeout_ms.is_none() {
                continue;
            }
            for model in &provider.models {
                map.insert(
                    model.id.clone(),
                    (provider.first_token_timeout_ms, provider.total_timeout_ms),
                );
            }
        }
        map
    }

    /// Build the provider specs used to construct the registry, resolving each
    /// `api_key_env` to its value from the environment.
    ///
    /// A missing env var yields `api_key = None` rather than a startup failure,
    /// so the gateway still boots (and `/health` still answers) without secrets;
    /// requests to that provider fail upstream with a clear error instead.
    #[must_use]
    pub fn provider_specs(&self) -> Vec<ProviderSpec> {
        self.providers
            .iter()
            .map(|p| ProviderSpec {
                name: p.name.clone(),
                kind: p.kind,
                api_key: p
                    .api_key_env
                    .as_ref()
                    .and_then(|var| std::env::var(var).ok()),
                base_url: p.base_url.clone(),
                models: p
                    .models
                    .iter()
                    .map(|m| ModelSpec {
                        id: m.id.clone(),
                        upstream_id: m.resolved_upstream_id().to_owned(),
                        capabilities: m.capabilities.clone(),
                        modalities: m.modalities.clone(),
                    })
                    .collect(),
            })
            .collect()
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
    fn duplicate_model_id_across_providers_cites_both() {
        let toml = r#"
            [[providers]]
            name = "provider-one"
            kind = "openai"
            [[providers.models]]
            id = "dup"
            capabilities = ["embed"]

            [[providers]]
            name = "provider-two"
            kind = "cohere"
            [[providers.models]]
            id = "dup"
            capabilities = ["rerank"]
        "#;
        let err = load_str(toml).unwrap_err();
        let msg = err.to_string();
        assert!(matches!(err, ConfigError::Validation { .. }));
        // The message names the colliding id AND both conflicting providers.
        assert!(msg.contains("dup"), "{msg}");
        assert!(msg.contains("provider-one"), "{msg}");
        assert!(msg.contains("provider-two"), "{msg}");
    }

    #[test]
    fn multiple_aliases_may_share_one_upstream_id() {
        // Two distinct public ids, both mapped to the same upstream model.
        let toml = r#"
            [[providers]]
            name = "openai"
            kind = "openai"
            [[providers.models]]
            id = "fast-embed"
            upstream_id = "text-embedding-3-small"
            capabilities = ["embed"]
            [[providers.models]]
            id = "cheap-embed"
            upstream_id = "text-embedding-3-small"
            capabilities = ["embed"]
        "#;
        let cfg = load_str(toml).unwrap();
        let models = &cfg.providers[0].models;
        assert_eq!(models[0].resolved_upstream_id(), "text-embedding-3-small");
        assert_eq!(models[1].resolved_upstream_id(), "text-embedding-3-small");
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
    fn auth_section_defaults_are_off_and_sane() {
        let cfg = load_str("").unwrap();
        assert!(!cfg.auth.enabled);
        assert_eq!(cfg.auth.flush_interval_ms, 10_000);
        assert_eq!(cfg.auth.usage_channel_capacity, 10_000);
        assert_eq!(cfg.auth.usage_batch_max, 500);
        assert_eq!(cfg.auth.usage_flush_ms, 2_000);
        assert_eq!(cfg.auth.retention_days, 30);
        assert!(cfg.telemetry.metadata_labels.is_empty());
    }

    #[test]
    fn enabled_auth_rejects_zero_knobs() {
        let err = load_str("[auth]\nenabled = true\nflush_interval_ms = 0\n").unwrap_err();
        assert!(err.to_string().contains("flush_interval_ms"));
        let err = load_str("[auth]\nenabled = true\nretention_days = 0\n").unwrap_err();
        assert!(err.to_string().contains("retention_days"));
    }

    #[test]
    fn disabled_auth_ignores_zero_knobs() {
        // The section is inert when disabled; don't block boot on it.
        assert!(load_str("[auth]\nenabled = false\nflush_interval_ms = 0\n").is_ok());
    }

    #[test]
    fn metadata_labels_must_be_valid_and_not_reserved() {
        let err = load_str("[telemetry]\nmetadata_labels = [\"not ok\"]\n").unwrap_err();
        assert!(err.to_string().contains("not ok"));
        let err = load_str("[telemetry]\nmetadata_labels = [\"model\"]\n").unwrap_err();
        assert!(err.to_string().contains("built-in"));
        let err = load_str("[telemetry]\nmetadata_labels = [\"team\", \"team\"]\n").unwrap_err();
        assert!(err.to_string().contains("duplicate"));
        assert!(load_str("[telemetry]\nmetadata_labels = [\"team\", \"env_1\"]\n").is_ok());
    }

    #[test]
    fn model_prices_parse_and_negative_prices_are_rejected() {
        let toml = r#"
            [[providers]]
            name = "openai"
            kind = "openai"
            [[providers.models]]
            id = "gpt"
            capabilities = ["chat"]
            cost_per_1m_input = 2.5
            cost_per_1m_output = 10.0
        "#;
        let cfg = load_str(toml).unwrap();
        assert_eq!(cfg.providers[0].models[0].cost_per_1m_input, Some(2.5));

        let bad = toml.replace("2.5", "-1.0");
        let err = load_str(&bad).unwrap_err();
        assert!(err.to_string().contains("cost_per_1m_input"));
    }

    #[test]
    fn missing_config_file_is_an_error_not_silent_defaults() {
        let err = Config::load(Path::new("/tmp/lumen-does-not-exist-xyz.toml")).unwrap_err();
        assert!(matches!(err, ConfigError::NotFound { .. }));
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn env_var_overrides_file_value() {
        // The closure must return `figment::Error`, whose size we don't control.
        #[allow(clippy::result_large_err)]
        figment::Jail::expect_with(|jail| {
            jail.create_file("config.toml", VALID)?;
            jail.set_env("LUMEN_SERVER__PORT", "9090");
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
    fn shipped_example_config_is_valid() {
        // Guards against the example rotting (a malformed example bit us before).
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../config.example.toml");
        let cfg = Config::load(Path::new(path)).expect("config.example.toml must parse");
        // Sanity: it exercises every rerank provider kind added in M3.
        for kind in [
            ProviderKind::Cohere,
            ProviderKind::Jina,
            ProviderKind::Voyage,
            ProviderKind::Tei,
        ] {
            assert!(
                cfg.providers.iter().any(|p| p.kind == kind),
                "example should demo {kind:?}"
            );
        }
    }

    #[test]
    fn resilience_defaults_are_sane_and_off() {
        let cfg = load_str("").unwrap();
        assert_eq!(cfg.resilience.retry_max_attempts, 3);
        assert_eq!(cfg.resilience.retry_base_ms, 200);
        assert_eq!(cfg.resilience.retry_max_ms, 5_000);
        assert_eq!(cfg.resilience.circuit_failure_threshold, 5);
        assert_eq!(cfg.resilience.circuit_cooldown_ms, 30_000);
        assert_eq!(cfg.resilience.connect_timeout_ms, 5_000);
        assert_eq!(cfg.resilience.total_timeout_ms, 600_000);
        assert!(!cfg.resilience.health_check_enabled);
    }

    #[test]
    fn resilience_rejects_zero_knobs() {
        let err = load_str("[resilience]\nretry_max_attempts = 0\n").unwrap_err();
        assert!(err.to_string().contains("retry_max_attempts"));
        let err = load_str("[resilience]\ntotal_timeout_ms = 0\n").unwrap_err();
        assert!(err.to_string().contains("total_timeout_ms"));
    }

    #[test]
    fn valid_fallback_chain_parses_and_maps() {
        let toml = r#"
            [[providers]]
            name = "openai"
            kind = "openai"
            [[providers.models]]
            id = "gpt"
            capabilities = ["chat"]
            fallbacks = ["claude"]

            [[providers]]
            name = "anthropic"
            kind = "anthropic"
            [[providers.models]]
            id = "claude"
            capabilities = ["chat"]
        "#;
        let cfg = load_str(toml).unwrap();
        let map = cfg.fallback_map();
        assert_eq!(map.get("gpt"), Some(&vec!["claude".to_owned()]));
        assert!(!map.contains_key("claude"));
    }

    #[test]
    fn fallback_to_unknown_model_is_rejected() {
        let toml = r#"
            [[providers]]
            name = "openai"
            kind = "openai"
            [[providers.models]]
            id = "gpt"
            capabilities = ["chat"]
            fallbacks = ["ghost"]
        "#;
        let err = load_str(toml).unwrap_err();
        assert!(err.to_string().contains("ghost"), "{err}");
    }

    #[test]
    fn fallback_missing_a_capability_is_rejected() {
        // The fallback serves only embed, but the model needs chat.
        let toml = r#"
            [[providers]]
            name = "openai"
            kind = "openai"
            [[providers.models]]
            id = "gpt"
            capabilities = ["chat"]
            fallbacks = ["embed-only"]
            [[providers.models]]
            id = "embed-only"
            capabilities = ["embed"]
        "#;
        let err = load_str(toml).unwrap_err();
        assert!(err.to_string().contains("capability"), "{err}");
    }

    #[test]
    fn self_fallback_is_rejected() {
        let toml = r#"
            [[providers]]
            name = "openai"
            kind = "openai"
            [[providers.models]]
            id = "gpt"
            capabilities = ["chat"]
            fallbacks = ["gpt"]
        "#;
        let err = load_str(toml).unwrap_err();
        assert!(err.to_string().contains("itself"), "{err}");
    }

    #[test]
    fn per_provider_timeout_overrides_parse_and_map() {
        let toml = r#"
            [[providers]]
            name = "slowvendor"
            kind = "openai"
            first_token_timeout_ms = 60000
            total_timeout_ms = 120000
            [[providers.models]]
            id = "gpt"
            capabilities = ["chat"]
        "#;
        let cfg = load_str(toml).unwrap();
        let overrides = cfg.model_timeout_overrides();
        assert_eq!(overrides.get("gpt"), Some(&(Some(60_000), Some(120_000))));
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
