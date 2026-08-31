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
use lumen_auth::events::SettingsOrigin;
use lumen_core::Capability;
use lumen_providers::{ModelSpec, ProviderKind, ProviderSpec};
use lumen_telemetry::logging::LogFormat;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// Top-level gateway configuration.
///
/// Every field belongs to exactly one of two layers (ADR 012): the
/// restart-only **boot layer** (server bind, log format, `auth.enabled` /
/// `auth.db_path`, and `config_source` itself - see [`BootView`]) or the
/// hot-reloadable **dynamic layer** (everything else). The classification is
/// exhaustive and pinned by a test (`every_config_field_is_classified_boot_or_dynamic`
/// in this module's `#[cfg(test)]`): a new top-level field must be added to
/// [`BootView`] and [`ensure_boot_only`] (if boot) or left out of both (if
/// dynamic), or that test fails.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// HTTP server settings. Boot layer.
    #[serde(default)]
    pub server: ServerConfig,
    /// Configured upstream providers. Dynamic layer.
    #[serde(default)]
    pub providers: Vec<ProviderConfig>,
    /// Log output format. Boot layer.
    #[serde(default)]
    pub log_format: LogFormatConfig,
    /// Virtual keys, budgets and usage logging (M5). Disabled by default.
    /// `enabled` and `db_path` are boot layer (a database connection cannot
    /// be swapped without a restart); every other `[auth]` knob is dynamic.
    #[serde(default)]
    pub auth: AuthConfig,
    /// Telemetry knobs (metadata label allowlist, ADR 002). Dynamic layer.
    #[serde(default)]
    pub telemetry: TelemetryConfig,
    /// Resilience knobs: retries, circuit breaker, timeouts, health checks (M6).
    /// Dynamic layer.
    #[serde(default)]
    pub resilience: ResilienceConfig,
    /// Guarded server-side image fetching for multimodal embeddings (M9).
    /// Dynamic layer.
    #[serde(default)]
    pub image_fetch: ImageFetchConfig,
    /// Local token-estimation strategy (ADR 003). Default: the byte
    /// heuristic. Dynamic layer.
    #[serde(default)]
    pub tokenizer: TokenizerConfig,
    /// Outbound webhooks for budget events (ADR 011). Absent by default:
    /// with no `[webhooks]` block the gateway makes no outbound call to
    /// anything but the configured providers. Dynamic layer.
    #[serde(default)]
    pub webhooks: Option<WebhooksConfig>,
    /// Where the dynamic config document lives (ADR 012): the file this
    /// process booted from (`"file"`, the default) or a database-backed
    /// source with a granular admin API (`"db"`, which requires
    /// `auth.enabled = true`). Boot layer: changing it needs a restart.
    #[serde(default)]
    pub config_source: ConfigSourceKind,
}

/// Where the dynamic config document lives (ADR 012 §1). Chosen at boot by
/// the `config_source` key; switching modes is a restart-time decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigSourceKind {
    /// The dynamic document lives in the boot TOML file itself (today's only
    /// mode): `PUT /admin/config` writes it back atomically and the file
    /// watcher hot-reloads on external edits.
    #[default]
    File,
    /// The dynamic document lives in the `config_versions` table of the auth
    /// database. Requires `auth.enabled = true`; the boot TOML may then hold
    /// only boot-layer keys (see [`ensure_boot_only`]).
    Db,
}

/// Outbound webhooks for key and group budget events (ADR 011).
///
/// The type itself lives in [`lumen_auth::events`] because four surfaces
/// share it: this `[webhooks]` block, the `PUT /admin/webhooks` request body,
/// the `webhook_config` database row, and the delivery pipeline built from any
/// of them. A field can therefore never mean one thing in a file and another
/// over the wire.
///
/// Strictly opt-in: the whole section is absent by default, and absent means
/// the gateway calls nothing but its providers (sovereignty pillar). When it
/// is present the gateway POSTs accounting facts (ids, names, budget figures)
/// to `url`; never a plaintext key, never request metadata, never prompt or
/// response content.
///
/// Settings written through `PUT /admin/webhooks` are stored in the database
/// and **win** over this block, so a runtime change is not undone by the next
/// reload (ADR 011 amendment §2). Every field is editable at runtime.
pub use lumen_auth::events::WebhookSettings as WebhooksConfig;

/// Opt-in accurate tokenizer (ADR 003). The estimation fallback (used only when
/// an upstream reports no usage) defaults to the cheap byte heuristic; set
/// `mode = "accurate"` for exact per-model BPE counting off the hot path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TokenizerConfig {
    /// Which local estimation strategy to use.
    #[serde(default)]
    pub mode: TokenizerMode,
}

/// Local token-estimation strategy for the ADR 003 fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenizerMode {
    /// The cheap ~4-bytes-per-token heuristic: zero cost, hot-path-safe.
    #[default]
    Heuristic,
    /// Exact per-model BPE (tiktoken) for OpenAI-family models, heuristic
    /// fallback for the rest. Runs on the blocking pool via `spawn_blocking`.
    Accurate,
}

/// Retries, circuit breaker, timeouts and background health checks (M6).
///
/// `first_token` is not here - it stays [`ServerConfig::first_token_timeout_ms`]
/// (its M4 home) and can be overridden per provider. `connect` here is the
/// default connect timeout for the shared, pooled HTTP client; a provider may
/// override it with `connect_timeout_ms`, which gives that provider its own
/// client (ADR 005, 2026-07-15 amendment).
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

/// The 5 hot-reloadable `[auth]` knobs (ADR 012 Task 8): every [`AuthConfig`]
/// field EXCEPT `enabled` and `db_path`, which are boot layer (a database
/// connection cannot be swapped without a restart) and so are absent from
/// this type entirely - `deny_unknown_fields` rejects a `PUT
/// /admin/config/auth` body naming either, rather than silently ignoring it.
/// Used only as the request/response shape for that granular endpoint; the
/// document itself still stores these fields inside the single `[auth]`
/// table alongside `enabled`/`db_path` (see
/// [`crate::config_edit::replace_auth_knobs`], which grafts a write from
/// this type into that table without touching the other two).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AuthDynamicKnobs {
    /// See [`AuthConfig::flush_interval_ms`].
    #[serde(default = "default_flush_interval_ms")]
    pub flush_interval_ms: u64,
    /// See [`AuthConfig::usage_channel_capacity`].
    #[serde(default = "default_usage_channel_capacity")]
    pub usage_channel_capacity: usize,
    /// See [`AuthConfig::usage_batch_max`].
    #[serde(default = "default_usage_batch_max")]
    pub usage_batch_max: usize,
    /// See [`AuthConfig::usage_flush_ms`].
    #[serde(default = "default_usage_flush_ms")]
    pub usage_flush_ms: u64,
    /// See [`AuthConfig::retention_days`].
    #[serde(default = "default_retention_days")]
    pub retention_days: u32,
}

impl From<&AuthConfig> for AuthDynamicKnobs {
    fn from(auth: &AuthConfig) -> Self {
        Self {
            flush_interval_ms: auth.flush_interval_ms,
            usage_channel_capacity: auth.usage_channel_capacity,
            usage_batch_max: auth.usage_batch_max,
            usage_flush_ms: auth.usage_flush_ms,
            retention_days: auth.retention_days,
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
    /// Azure OpenAI `api-version` (the `azure` kind only; setting it on any
    /// other kind is a boot-time validation error). Takes precedence over an
    /// `?api-version=...` query string on `base_url` (kept for back-compat),
    /// which takes precedence over the built-in default (issue #65).
    #[serde(default)]
    pub api_version: Option<String>,
    /// Per-provider first-token timeout override in ms (else the global
    /// [`ServerConfig::first_token_timeout_ms`]).
    #[serde(default)]
    pub first_token_timeout_ms: Option<u64>,
    /// Per-provider total timeout override in ms (else the global
    /// [`ResilienceConfig::total_timeout_ms`]).
    #[serde(default)]
    pub total_timeout_ms: Option<u64>,
    /// Reject requests that set an unsupported-but-meaningful field instead of
    /// silently dropping it: strict returns a 400 (`LM-1001`); lenient (the
    /// default) drops the field with a debug log. Honored by Ollama for
    /// `dimensions` (issue #25) and by the translated chat providers
    /// (`anthropic`, `google`, `vertex_ai`, `bedrock`, `cohere`) for OpenAI
    /// chat fields their upstream schema cannot express - e.g.
    /// `response_format`/`seed`/`logprobs` on Anthropic (issue #72). See
    /// `docs/providers.md` for the per-provider field matrix.
    #[serde(default)]
    pub strict: bool,
    /// Per-provider connection-establishment timeout override in ms (else the
    /// global [`ResilienceConfig::connect_timeout_ms`]). Setting it gives this
    /// provider its OWN HTTP client, so it no longer shares the process-wide
    /// connection pool (ADR 005, 2026-07-15 amendment): a deliberate trade-off
    /// for an upstream that needs a tighter or looser connect deadline than the
    /// rest.
    #[serde(default)]
    pub connect_timeout_ms: Option<u64>,
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

/// Per-provider knob validation: timeout overrides must not be 0, and
/// `api_version` must be non-blank, carry no leading/trailing whitespace
/// (a padded value would be percent-encoded verbatim into the request URL
/// and fail upstream with an Azure 4xx only at request time), and only
/// appear on the `azure` kind (issue #65) - every other kind would silently
/// ignore it, so a boot-time error beats a misconfigured provider
/// discovered at request time.
fn validate_provider_knobs(
    provider: &ProviderConfig,
    err: &impl Fn(String) -> ConfigError,
) -> Result<(), ConfigError> {
    for (field, value) in [
        ("first_token_timeout_ms", provider.first_token_timeout_ms),
        ("total_timeout_ms", provider.total_timeout_ms),
        ("connect_timeout_ms", provider.connect_timeout_ms),
    ] {
        if value == Some(0) {
            return Err(err(format!(
                "provider '{}': {field} must not be 0",
                provider.name
            )));
        }
    }
    if let Some(api_version) = &provider.api_version {
        if api_version.is_empty() || api_version.trim() != api_version {
            return Err(err(format!(
                "provider '{}': api_version must be non-empty with no leading or \
                 trailing whitespace",
                provider.name
            )));
        }
        if provider.kind != ProviderKind::Azure {
            return Err(err(format!(
                "provider '{}': api_version is only supported by kind 'azure' \
                 (kind '{}' would ignore it)",
                provider.name,
                provider.kind.as_str()
            )));
        }
    }
    Ok(())
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
    /// A boot-only document (the `config_source = "db"` boot TOML) named a
    /// dynamic-layer key (ADR 012 §1): never silently ignored as a second
    /// source of that key, always a boot error.
    #[error(
        "boot config '{path}': unexpected key '{key}' in boot-only document: remove it from \
         the boot file, or set config_source = \"file\""
    )]
    DynamicKeyInBootConfig {
        /// The boot document's path or label.
        path: String,
        /// The offending key, dotted for a nested field (e.g.
        /// `auth.flush_interval_ms`) or bare for a top-level one (e.g.
        /// `providers`).
        key: String,
    },
}

/// Render a `figment` extraction error without the trailing `" in {source}
/// {name}"` fragment its own `Display` appends (e.g. `" in /srv/lumen/lumen
/// .toml.4127-0.tmp TOML file"`): `source` is the path figment actually read,
/// which is the real config path on a normal boot but the staged `.tmp` file
/// during `PUT /admin/config` validation (`crate::reload::validate_candidate`
/// runs `Config::load` against the staging copy). `ConfigError::Parse`
/// already carries the correct path in its own `path` field, so repeating
/// figment's copy is redundant on a normal load and a filesystem-path leak on
/// a `PUT` rejection.
///
/// Reconstructed structurally from `figment::Error`'s public fields (`kind`,
/// `path`, `profile`, `metadata`), not by trimming the formatted string: this
/// stays correct if figment ever reorders or restyles its own `Display`,
/// where a suffix-trim would silently stop matching.
fn describe_figment_error(error: &figment::Error) -> String {
    use std::fmt::Write as _;

    error
        .clone()
        .into_iter()
        .map(|level| {
            let mut message = level.kind.to_string();
            if let (Some(profile), Some(metadata)) = (&level.profile, &level.metadata) {
                if !level.path.is_empty() {
                    let key = metadata.interpolate(profile, &level.path);
                    // `write!` into a `String` is infallible.
                    let _ = write!(message, " for key {key:?}");
                }
            }
            message
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The `LUMEN_`-prefixed environment variables that are process **secrets**,
/// not config fields, and so must be excluded from figment's env overlay.
///
/// `Config` denies unknown fields, so a `LUMEN_`-prefixed variable that does
/// not name a config key makes every load fail - `--check-config` and real
/// boots alike - with an "unknown field" parse error. Two such variables
/// exist:
///
/// * `LUMEN_MASTER_KEY`, read by `boot_auth_stack`, whose name is fixed.
/// * The webhook signing secret (ADR 011), whose name is **not** fixed: the
///   operator chooses it in `webhooks.signing_key_env`, and the ADR's own
///   example names it `LUMEN_WEBHOOK_SECRET`. It is discovered here by peeking
///   at the TOML file alone: a permissive parse of that single key, with no
///   env overlay and no validation, so a malformed file still produces the
///   real error from the full load rather than one from this peek.
///
/// Only the exact variable named in config is excluded, so a typo elsewhere in
/// the `LUMEN_*` namespace is still caught. A name containing `__` is not
/// excluded (figment would read it as a nested key); config validation rejects
/// that shape outright.
fn secret_env_keys(path: &Path) -> Vec<String> {
    secret_env_keys_with_dynamic(path, "")
}

/// [`secret_env_keys`], extended for `config_source = "db"` boot: the
/// `[webhooks]` block lives in the *dynamic* document there (ADR 012), not in
/// the boot file, so the signing-variable peek must also scan `dynamic_toml`.
/// `Config::load` (file mode) calls this with `dynamic_toml = ""`, so its
/// behavior is unchanged; `Config::load_with_dynamic` (db mode) passes the
/// real dynamic text.
fn secret_env_keys_with_dynamic(path: &Path, dynamic_toml: &str) -> Vec<String> {
    let peek_figment = Figment::new()
        .merge(Toml::file(path))
        .merge(Toml::string(dynamic_toml));
    secret_env_keys_from_figment(&peek_figment)
}

/// [`secret_env_keys`], for a candidate document that exists only as TEXT, not
/// yet on disk: [`Config::load_text`] validates exactly the bytes about to be
/// persisted (the `PUT /admin/config` candidate in file mode), so the
/// signing-variable peek must run over that text directly rather than a file
/// path.
fn secret_env_keys_from_text(text: &str) -> Vec<String> {
    let peek_figment = Figment::new().merge(Toml::string(text));
    secret_env_keys_from_figment(&peek_figment)
}

/// Shared peek logic behind [`secret_env_keys_with_dynamic`] and
/// [`secret_env_keys_from_text`]: find the webhook signing variable's name (if
/// any) in an already-assembled figment, so it can be excluded from the real
/// `LUMEN_` env overlay.
fn secret_env_keys_from_figment(peek_figment: &Figment) -> Vec<String> {
    /// Just enough of the config to find the signing variable's name.
    #[derive(Deserialize)]
    struct Peek {
        webhooks: Option<PeekWebhooks>,
    }
    #[derive(Deserialize)]
    struct PeekWebhooks {
        signing_key_env: Option<String>,
    }

    let mut keys = vec!["master_key".to_owned()];
    if let Ok(peek) = peek_figment.extract::<Peek>() {
        if let Some(var) = peek.webhooks.and_then(|w| w.signing_key_env) {
            if let Some(suffix) = var.strip_prefix("LUMEN_") {
                keys.push(suffix.to_lowercase());
            }
        }
    }
    keys
}

/// The boot-layer fields of [`Config`] (ADR 012 §1), snapshotted from a TOML
/// document with no env overlay and no semantic validation - a pure
/// change-detector over the document text, not a boot-readiness check.
///
/// Field-for-field mirror of `Config`'s boot-classified fields; see the
/// classification note on [`Config`] itself. An explicit value equal to the
/// default is indistinguishable from an absent one (figment resolves both to
/// the same `Config`), which is the intended behavior: `boot_layer_diff`
/// answers "would a restart see a different effective boot config", not "did
/// the byte text change".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootView {
    /// [`ServerConfig::host`].
    pub host: String,
    /// [`ServerConfig::port`].
    pub port: u16,
    /// [`ServerConfig::body_limit`].
    pub body_limit: usize,
    /// [`ServerConfig::first_token_timeout_ms`].
    pub first_token_timeout_ms: u64,
    /// [`ServerConfig::sse_heartbeat_ms`].
    pub sse_heartbeat_ms: u64,
    /// [`Config::log_format`].
    pub log_format: LogFormatConfig,
    /// [`AuthConfig::enabled`].
    pub auth_enabled: bool,
    /// [`AuthConfig::db_path`].
    pub db_path: String,
    /// [`Config::config_source`].
    pub config_source: ConfigSourceKind,
}

/// Snapshot the boot-layer fields of the document at `toml_text` (see
/// [`BootView`]). Parses with `Toml::string` only - deliberately no
/// `LUMEN_*` env overlay, since a boot-layer diff compares two documents and
/// the env overlay would apply identically to both sides - and extracts a
/// full [`Config`] WITHOUT running [`Config::validate`]: this is a change
/// detector, not a readiness check, so a document that is individually
/// invalid (e.g. a boot-only file missing every dynamic default) must still
/// produce a view to diff against.
pub fn boot_view(toml_text: &str, label: &str) -> Result<BootView, ConfigError> {
    let figment = Figment::new().merge(Toml::string(toml_text));
    let config: Config = figment.extract().map_err(|e| ConfigError::Parse {
        path: label.to_owned(),
        message: describe_figment_error(&e),
    })?;
    Ok(BootView {
        host: config.server.host,
        port: config.server.port,
        body_limit: config.server.body_limit,
        first_token_timeout_ms: config.server.first_token_timeout_ms,
        sse_heartbeat_ms: config.server.sse_heartbeat_ms,
        log_format: config.log_format,
        auth_enabled: config.auth.enabled,
        db_path: config.auth.db_path,
        config_source: config.config_source,
    })
}

/// Dotted key names of every boot-layer field that differs between `current`
/// and `candidate` (e.g. `server.port`, `auth.db_path`, `config_source`).
/// Empty means a restart would boot into an equivalent boot configuration -
/// a dynamic-only change (providers, resilience, ...) never appears here.
pub fn boot_layer_diff(current: &str, candidate: &str) -> Result<Vec<String>, ConfigError> {
    let before = boot_view(current, "current")?;
    let after = boot_view(candidate, "candidate")?;
    let mut diffs = Vec::new();
    if before.host != after.host {
        diffs.push("server.host".to_owned());
    }
    if before.port != after.port {
        diffs.push("server.port".to_owned());
    }
    if before.body_limit != after.body_limit {
        diffs.push("server.body_limit".to_owned());
    }
    if before.first_token_timeout_ms != after.first_token_timeout_ms {
        diffs.push("server.first_token_timeout_ms".to_owned());
    }
    if before.sse_heartbeat_ms != after.sse_heartbeat_ms {
        diffs.push("server.sse_heartbeat_ms".to_owned());
    }
    if before.log_format != after.log_format {
        diffs.push("log_format".to_owned());
    }
    if before.auth_enabled != after.auth_enabled {
        diffs.push("auth.enabled".to_owned());
    }
    if before.db_path != after.db_path {
        diffs.push("auth.db_path".to_owned());
    }
    if before.config_source != after.config_source {
        diffs.push("config_source".to_owned());
    }
    Ok(diffs)
}

/// Top-level keys a boot-only document (`config_source = "db"`) may contain.
const BOOT_ONLY_TOP_LEVEL_KEYS: [&str; 4] = ["server", "log_format", "auth", "config_source"];

/// Keys allowed inside `[auth]` in a boot-only document - the rest of
/// `AuthConfig` is dynamic (see the classification note on [`Config`]).
const BOOT_ONLY_AUTH_KEYS: [&str; 2] = ["enabled", "db_path"];

/// Reject any key in `toml_text` that is not boot-layer (ADR 012 §1): in
/// `config_source = "db"` mode the boot file may hold ONLY `server`,
/// `log_format`, `auth.enabled`, `auth.db_path` and `config_source` itself -
/// a dynamic key there would be a second, silently-diverging source for a
/// value the DB is supposed to own exclusively, which is exactly the
/// drift-prone pattern ADR 012 refuses. Parses with plain `toml::Value` (not
/// `Config`) so a document that is boot-only but otherwise dynamically
/// invalid still gets this specific, actionable error instead of a generic
/// parse failure.
pub fn ensure_boot_only(toml_text: &str, label: &str) -> Result<(), ConfigError> {
    let value: toml::Value =
        toml_text
            .parse()
            .map_err(|e: toml::de::Error| ConfigError::Parse {
                path: label.to_owned(),
                message: e.to_string(),
            })?;
    let Some(table) = value.as_table() else {
        // A syntactically valid TOML document is always a table at the top
        // level; `toml::Value::parse` cannot produce anything else here.
        return Ok(());
    };
    for (key, entry) in table {
        if !BOOT_ONLY_TOP_LEVEL_KEYS.contains(&key.as_str()) {
            return Err(ConfigError::DynamicKeyInBootConfig {
                path: label.to_owned(),
                key: key.clone(),
            });
        }
        if key == "auth" {
            if let Some(auth_table) = entry.as_table() {
                for auth_key in auth_table.keys() {
                    if !BOOT_ONLY_AUTH_KEYS.contains(&auth_key.as_str()) {
                        return Err(ConfigError::DynamicKeyInBootConfig {
                            path: label.to_owned(),
                            key: format!("auth.{auth_key}"),
                        });
                    }
                }
            }
        }
    }
    Ok(())
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
        // Secrets that live in `LUMEN_`-prefixed variables are read directly
        // from the process environment, never through the config, and must be
        // kept out of the figment overlay - see [`secret_env_keys`].
        let secrets = secret_env_keys(path);
        let ignored: Vec<&str> = secrets.iter().map(String::as_str).collect();
        let figment = Figment::new()
            .merge(Toml::file(path))
            .merge(Env::prefixed("LUMEN_").ignore(&ignored).split("__"));
        Self::from_figment(&figment, &label)
    }

    /// Load and validate configuration for `config_source = "db"` boot
    /// (ADR 012): `boot_path` supplies the boot layer (server bind, log
    /// format, `auth.enabled` / `auth.db_path`, `config_source`), and
    /// `dynamic_toml` (the current document from a
    /// [`crate::config_source::DbSource`]) supplies everything else
    /// (providers, resilience, telemetry, webhooks, ...). The two merge into
    /// one `Config` the same way the single file does in file mode, then
    /// `LUMEN_*` env vars overlay both, exactly as [`Self::load`]. Boot-layer
    /// values always come from `boot_path`: nothing in this function stops
    /// `dynamic_toml` from also naming a boot key, but the write path that
    /// produces `dynamic_toml` (`ensure_boot_only` applied to admin writes)
    /// is responsible for never letting one land there.
    pub fn load_with_dynamic(boot_path: &Path, dynamic_toml: &str) -> Result<Self, ConfigError> {
        let label = boot_path.display().to_string();
        // Mirrors `Self::load`: an explicitly requested boot file that does
        // not exist is an error, not a silent fall-through to defaults.
        if !boot_path.exists() {
            return Err(ConfigError::NotFound { path: label });
        }
        let secrets = secret_env_keys_with_dynamic(boot_path, dynamic_toml);
        let ignored: Vec<&str> = secrets.iter().map(String::as_str).collect();
        let figment = Figment::new()
            .merge(Toml::file(boot_path))
            .merge(Toml::string(dynamic_toml))
            .merge(Env::prefixed("LUMEN_").ignore(&ignored).split("__"));
        Self::from_figment(&figment, &label)
    }

    /// Text-based sibling of [`Self::load`] (ADR 012): parse and validate
    /// `text` as a self-contained config document (`Toml::string`, never a
    /// file read), overlaid with the same `LUMEN_*` environment variables.
    ///
    /// Used by [`crate::config_source::ConfigContext::validate_document`] in
    /// file mode to validate exactly the candidate bytes a `PUT
    /// /admin/config` is about to persist, without ever staging them to disk
    /// first (a candidate that never becomes a temp file can never leak that
    /// temp file's path into an error message). `label` names the document in
    /// any error message - never a filesystem path derived from `text`
    /// itself, since a candidate string has none.
    pub fn load_text(text: &str, label: &str) -> Result<Self, ConfigError> {
        let secrets = secret_env_keys_from_text(text);
        let ignored: Vec<&str> = secrets.iter().map(String::as_str).collect();
        let figment = Figment::new()
            .merge(Toml::string(text))
            .merge(Env::prefixed("LUMEN_").ignore(&ignored).split("__"));
        Self::from_figment(&figment, label)
    }

    /// Build a config from an arbitrary figment (used by tests) and validate it.
    fn from_figment(figment: &Figment, path_label: &str) -> Result<Self, ConfigError> {
        let config: Config = figment.extract().map_err(|e| ConfigError::Parse {
            path: path_label.to_owned(),
            message: describe_figment_error(&e),
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

        if let Some(webhooks) = &self.webhooks {
            // Every event kind is about a virtual key or a budget group, so
            // the block is meaningless - and silently inert - without auth.
            // Say so at boot rather than shipping a dead integration.
            if !self.auth.enabled {
                return Err(err(
                    "[webhooks] requires auth.enabled = true: every budget event describes a \
                     virtual key or a budget group"
                        .to_owned(),
                ));
            }
            webhooks
                .validate(SettingsOrigin::ConfigFile)
                .map_err(&err)?;
        }

        self.telemetry.validate(path_label)?;
        self.resilience.validate(path_label)?;

        for provider in &self.providers {
            validate_provider_knobs(provider, &err)?;
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
                api_version: p.api_version.clone(),
                strict: p.strict,
                connect_timeout_ms: p.connect_timeout_ms,
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
    use lumen_auth::events::EventKind;

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

    // ---- Outbound budget webhooks (ADR 011) --------------------------------

    /// An `[auth]` section webhooks can legally hang off, plus one provider.
    const AUTH_ON: &str = r#"
        [auth]
        enabled = true

        [[providers]]
        name = "openai-main"
        kind = "openai"

        [[providers.models]]
        id = "gpt-4o"
        capabilities = ["chat"]
    "#;

    fn with_webhooks(block: &str) -> Result<Config, ConfigError> {
        load_str(&format!("{AUTH_ON}\n[webhooks]\n{block}"))
    }

    #[test]
    fn a_lumen_prefixed_signing_env_var_is_never_folded_into_the_config() {
        // ADR 011's own example names the secret LUMEN_WEBHOOK_SECRET, which
        // sits inside figment's `LUMEN_` overlay namespace. Without excluding
        // it by name, setting the very variable the config asks for made every
        // load fail with "unknown field: found `webhook_secret`" - a gateway
        // that refuses to boot the moment webhooks are actually configured.
        #[allow(clippy::result_large_err)]
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "config.toml",
                &format!(
                    "{AUTH_ON}\n[webhooks]\nurl = \"https://b.example/e\"\n\
                     signing_key_env = \"LUMEN_WEBHOOK_SECRET\"\n"
                ),
            )?;
            jail.set_env("LUMEN_WEBHOOK_SECRET", "whsec-do-not-fold-me");
            jail.set_env("LUMEN_MASTER_KEY", "a".repeat(64));

            let cfg = Config::load(Path::new("config.toml")).expect("must load");
            let hooks = cfg.webhooks.as_ref().expect("block present");
            // The NAME survives; the value never enters the config at all.
            assert_eq!(
                hooks.signing_key_env.as_deref(),
                Some("LUMEN_WEBHOOK_SECRET")
            );
            let rendered = format!("{cfg:?}");
            assert!(!rendered.contains("whsec-do-not-fold-me"), "{rendered}");
            // Sanity: the rest of the env-override mechanism still works.
            jail.set_env("LUMEN_SERVER__PORT", "9191");
            let cfg = Config::load(Path::new("config.toml")).expect("must load");
            assert_eq!(cfg.server.port, 9191);
            Ok(())
        });
    }

    #[test]
    fn only_the_configured_signing_var_is_excluded_from_the_overlay() {
        // The exclusion is by exact name, so an unrelated LUMEN_ typo is still
        // caught rather than silently swallowed. Asserted against the
        // exclusion list itself rather than by setting a bogus environment
        // variable: `figment::Jail` restores the process environment on drop,
        // but tests share that environment while they run, so a deliberately
        // load-breaking variable would leak into whatever runs beside it.
        #[allow(clippy::result_large_err)]
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "config.toml",
                &format!(
                    "{AUTH_ON}\n[webhooks]\nurl = \"https://b.example/e\"\n\
                     signing_key_env = \"LUMEN_WEBHOOK_SECRET\"\n"
                ),
            )?;
            let excluded = secret_env_keys(Path::new("config.toml"));
            assert_eq!(excluded, ["master_key", "webhook_secret"]);
            Ok(())
        });
    }

    #[test]
    fn a_signing_var_outside_the_lumen_namespace_needs_no_exclusion() {
        // A name the config loader never looks at does not have to be kept out
        // of the overlay, so it must not bloat the exclusion list.
        #[allow(clippy::result_large_err)]
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "config.toml",
                &format!(
                    "{AUTH_ON}\n[webhooks]\nurl = \"https://b.example/e\"\n\
                     signing_key_env = \"BILLING_WEBHOOK_SECRET\"\n"
                ),
            )?;
            assert_eq!(secret_env_keys(Path::new("config.toml")), ["master_key"]);
            Ok(())
        });
    }

    #[test]
    fn a_lumen_prefixed_signing_var_containing_a_double_underscore_is_rejected() {
        // figment splits LUMEN_ variables into nested keys on `__`, which no
        // by-name exclusion can match - so the shape is refused at validation
        // instead of producing a baffling "unknown field" at boot.
        let err = with_webhooks(
            "url = \"https://b.example/e\"\nsigning_key_env = \"LUMEN_WEBHOOK__SECRET\"",
        )
        .unwrap_err();
        let ConfigError::Validation { message, .. } = err else {
            panic!("expected a validation error");
        };
        assert!(message.contains("must not contain '__'"), "{message}");

        // A non-LUMEN_ name may contain anything: it never enters the overlay.
        assert!(with_webhooks(
            "url = \"https://b.example/e\"\nsigning_key_env = \"MY_APP__SECRET\""
        )
        .is_ok());
    }

    #[test]
    fn no_webhooks_block_means_no_webhooks() {
        // The sovereignty default: absent, not merely disabled.
        assert!(load_str(VALID).unwrap().webhooks.is_none());
        assert!(load_str("").unwrap().webhooks.is_none());
    }

    #[test]
    fn a_webhooks_block_parses_with_its_documented_defaults() {
        let cfg = with_webhooks(r#"url = "https://backend.example.com/lumen/events""#).unwrap();
        let hooks = cfg.webhooks.expect("block present");
        assert_eq!(hooks.url, "https://backend.example.com/lumen/events");
        assert_eq!(hooks.signing_key_env, None);
        assert_eq!(
            hooks.events,
            [EventKind::BudgetThreshold, EventKind::BudgetExhausted]
        );
        assert_eq!(hooks.thresholds, [50, 80, 95]);
        assert_eq!(hooks.channel_capacity, 1_024);
        assert_eq!(hooks.timeout_ms, 5_000);
        assert_eq!(hooks.max_attempts, 5);
        assert_eq!(hooks.retry_base_ms, 500);
    }

    #[test]
    fn every_event_kind_is_addressable_by_its_dotted_name() {
        let cfg = with_webhooks(
            r#"
            url = "https://b.example/e"
            events = ["budget.threshold", "budget.exhausted", "key.disabled", "key.rotated", "key.deleted"]
            "#,
        )
        .unwrap();
        assert_eq!(cfg.webhooks.expect("block present").events, EventKind::ALL);
    }

    #[test]
    fn an_unknown_event_kind_is_rejected() {
        let err = with_webhooks(
            r#"
            url = "https://b.example/e"
            events = ["budget.almost"]
            "#,
        )
        .unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }), "{err:?}");
    }

    #[test]
    fn webhooks_require_auth_to_be_enabled() {
        // Every event describes a key or a group, so the block is inert
        // without auth - say so at boot instead of shipping a dead hook.
        let err = load_str("[webhooks]\nurl = \"https://b.example/e\"\n").unwrap_err();
        let ConfigError::Validation { message, .. } = err else {
            panic!("expected a validation error");
        };
        assert!(message.contains("auth.enabled"), "{message}");
    }

    #[test]
    fn a_webhook_url_must_be_a_non_blank_http_url() {
        for (block, expected) in [
            ("url = \"\"", "must not be empty"),
            ("url = \" https://b.example/e \"", "whitespace"),
            ("url = \"ftp://b.example/e\"", "http:// or https://"),
            ("url = \"b.example/e\"", "http:// or https://"),
        ] {
            let err = with_webhooks(block).unwrap_err();
            let ConfigError::Validation { message, .. } = err else {
                panic!("expected a validation error for {block}");
            };
            assert!(message.contains(expected), "{block}: {message}");
        }
    }

    #[test]
    fn an_empty_event_list_is_rejected_rather_than_treated_as_off() {
        let err = with_webhooks(
            r#"
            url = "https://b.example/e"
            events = []
            "#,
        )
        .unwrap_err();
        let ConfigError::Validation { message, .. } = err else {
            panic!("expected a validation error");
        };
        assert!(message.contains("at least one event kind"), "{message}");
    }

    #[test]
    fn thresholds_are_only_required_when_the_threshold_event_is_enabled() {
        // Enabled and empty: an error, because nothing would ever fire.
        let err = with_webhooks(
            r#"
            url = "https://b.example/e"
            events = ["budget.threshold"]
            thresholds = []
            "#,
        )
        .unwrap_err();
        let ConfigError::Validation { message, .. } = err else {
            panic!("expected a validation error");
        };
        assert!(
            message.contains("thresholds must not be empty"),
            "{message}"
        );

        // Not enabled: an empty list is fine.
        assert!(with_webhooks(
            r#"
            url = "https://b.example/e"
            events = ["budget.exhausted"]
            thresholds = []
            "#,
        )
        .is_ok());
    }

    #[test]
    fn thresholds_must_be_percentages_in_1_to_100() {
        for bad in ["0", "101"] {
            let err = with_webhooks(&format!(
                "url = \"https://b.example/e\"\nthresholds = [50, {bad}]"
            ))
            .unwrap_err();
            let ConfigError::Validation { message, .. } = err else {
                panic!("expected a validation error for {bad}");
            };
            assert!(message.contains("1..=100"), "{bad}: {message}");
        }
        // 100 % is legal: an operator may want a signal exactly at the cap.
        assert!(with_webhooks("url = \"https://b.example/e\"\nthresholds = [100]").is_ok());
    }

    #[test]
    fn zero_valued_webhook_knobs_are_rejected() {
        for field in [
            "channel_capacity",
            "timeout_ms",
            "max_attempts",
            "retry_base_ms",
        ] {
            let err =
                with_webhooks(&format!("url = \"https://b.example/e\"\n{field} = 0")).unwrap_err();
            let ConfigError::Validation { message, .. } = err else {
                panic!("expected a validation error for {field}");
            };
            assert!(message.contains(field), "{field}: {message}");
        }
    }

    #[test]
    fn a_blank_signing_key_env_name_is_rejected() {
        for value in ["\"\"", "\"  \"", "\" LUMEN_WEBHOOK_SECRET \""] {
            let err = with_webhooks(&format!(
                "url = \"https://b.example/e\"\nsigning_key_env = {value}"
            ))
            .unwrap_err();
            let ConfigError::Validation { message, .. } = err else {
                panic!("expected a validation error for {value}");
            };
            assert!(message.contains("signing_key_env"), "{value}: {message}");
        }
    }

    #[test]
    fn an_unknown_webhook_field_is_rejected_and_named() {
        let err = with_webhooks("url = \"https://b.example/e\"\nurll = \"typo\"").unwrap_err();
        let ConfigError::Parse { message, .. } = err else {
            panic!("expected a parse error");
        };
        assert!(message.contains("urll"), "{message}");
    }

    #[test]
    fn the_webhook_block_never_holds_a_secret_only_an_env_var_name() {
        // Same contract as provider keys: a config file (and so a `GET
        // /admin/config` response) can never contain the signing secret.
        let cfg = with_webhooks(
            r#"
            url = "https://b.example/e"
            signing_key_env = "LUMEN_WEBHOOK_SECRET"
            "#,
        )
        .unwrap();
        let rendered = format!("{:?}", cfg.webhooks.as_ref().expect("block present"));
        assert!(rendered.contains("LUMEN_WEBHOOK_SECRET"), "{rendered}");
        // Serialising the config (what a control plane would receive) carries
        // the variable NAME and nothing more.
        let serialized = serde_json::to_string(&cfg).expect("config serializes");
        assert!(serialized.contains("LUMEN_WEBHOOK_SECRET"), "{serialized}");
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

    /// Regression test: `figment::Error`'s own `Display` appends
    /// `" in {source} {name}"` (e.g. `" in /tmp/x/bad.toml TOML file"`) to a
    /// parse failure. `ConfigError::Parse.message` used to be that full
    /// string verbatim, so a caller who only has `message` (not `path`) -
    /// exactly the situation `admin::describe_rejection` is in when scrubbing
    /// a `PUT /admin/config` rejection - saw figment's own copy of the path,
    /// which is the STAGED `.tmp` file during a `PUT`, not anything the
    /// operator wrote. `Config::load` on a real file (not `Toml::string`, so
    /// figment's `source` is a genuine path) exercises this directly.
    #[test]
    fn parse_error_message_has_no_redundant_path_but_keeps_the_position() {
        let dir = std::env::temp_dir().join(format!(
            "lumen-config-parse-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("bad.toml");
        std::fs::write(&path, "this is not valid toml {{{").expect("write bad config");

        let err = Config::load(&path).expect_err("malformed TOML must be rejected");
        let ConfigError::Parse { message, .. } = &err else {
            panic!("expected a Parse error, got {err:?}");
        };
        assert!(
            !message.contains(&path.display().to_string()),
            "the message must not repeat figment's own copy of the file path: {message}"
        );
        assert!(
            !message.contains("TOML file"),
            "the message must not carry figment's source-kind tag: {message}"
        );
        assert!(
            message.to_lowercase().contains("line"),
            "the message must still name the offending line/column: {message}"
        );

        let _ = std::fs::remove_dir_all(&dir);
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
    fn master_key_env_var_is_never_folded_into_the_config() {
        // LUMEN_MASTER_KEY is a secret consumed directly by `boot_auth_stack`
        // via `std::env::var`, never a config field. Setting it (as any real
        // `auth.enabled = true` deployment must) previously made `Config::load`
        // - and therefore `--check-config` and every real boot - fail with
        // "unknown field: found 'master_key'" because `Config` denies unknown
        // fields. This must load cleanly and must not surface `master_key`
        // anywhere in the parsed config.
        #[allow(clippy::result_large_err)]
        figment::Jail::expect_with(|jail| {
            jail.create_file("config.toml", VALID)?;
            jail.set_env(
                "LUMEN_MASTER_KEY",
                "a".repeat(64), // 64 hex chars, matches the real format
            );
            let cfg = Config::load(Path::new("config.toml")).unwrap();
            // Sanity: the rest of the env-override mechanism still works.
            assert_eq!(cfg.server.port, 8080);
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
    fn per_provider_connect_timeout_parses_and_reaches_the_spec() {
        let toml = r#"
            [[providers]]
            name = "flakyvendor"
            kind = "openai"
            connect_timeout_ms = 250
            [[providers.models]]
            id = "gpt"
            capabilities = ["chat"]
        "#;
        let cfg = load_str(toml).unwrap();
        assert_eq!(cfg.providers[0].connect_timeout_ms, Some(250));
        let specs = cfg.provider_specs();
        assert_eq!(specs[0].connect_timeout_ms, Some(250));
    }

    #[test]
    fn per_provider_connect_timeout_of_zero_is_rejected() {
        let toml = r#"
            [[providers]]
            name = "vendor"
            kind = "openai"
            connect_timeout_ms = 0
            [[providers.models]]
            id = "gpt"
            capabilities = ["chat"]
        "#;
        let err = load_str(toml).unwrap_err();
        assert!(err.to_string().contains("connect_timeout_ms"));
    }

    #[test]
    fn azure_api_version_parses_and_reaches_the_spec() {
        let toml = r#"
            [[providers]]
            name = "azure"
            kind = "azure"
            base_url = "https://my-resource.openai.azure.com"
            api_version = "2025-01-01-preview"
            [[providers.models]]
            id = "gpt"
            capabilities = ["chat"]
        "#;
        let cfg = load_str(toml).unwrap();
        assert_eq!(
            cfg.providers[0].api_version.as_deref(),
            Some("2025-01-01-preview")
        );
        let specs = cfg.provider_specs();
        assert_eq!(specs[0].api_version.as_deref(), Some("2025-01-01-preview"));
    }

    #[test]
    fn azure_api_version_field_coexists_with_a_base_url_query_string() {
        // Back-compat: both forms parse together; the provider gives the
        // explicit field precedence over the query-string value at runtime.
        let toml = r#"
            [[providers]]
            name = "azure"
            kind = "azure"
            base_url = "https://my-resource.openai.azure.com?api-version=2023-05-15"
            api_version = "2025-01-01-preview"
            [[providers.models]]
            id = "gpt"
            capabilities = ["chat"]
        "#;
        let cfg = load_str(toml).unwrap();
        let specs = cfg.provider_specs();
        assert_eq!(specs[0].api_version.as_deref(), Some("2025-01-01-preview"));
        assert_eq!(
            specs[0].base_url.as_deref(),
            Some("https://my-resource.openai.azure.com?api-version=2023-05-15")
        );
    }

    #[test]
    fn api_version_on_a_non_azure_kind_is_rejected() {
        let toml = r#"
            [[providers]]
            name = "openai"
            kind = "openai"
            api_version = "2025-01-01-preview"
            [[providers.models]]
            id = "gpt"
            capabilities = ["chat"]
        "#;
        let err = load_str(toml).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("api_version"), "{msg}");
        assert!(msg.contains("azure"), "{msg}");
    }

    #[test]
    fn blank_api_version_is_rejected() {
        let toml = r#"
            [[providers]]
            name = "azure"
            kind = "azure"
            base_url = "https://my-resource.openai.azure.com"
            api_version = " "
            [[providers.models]]
            id = "gpt"
            capabilities = ["chat"]
        "#;
        let err = load_str(toml).unwrap_err();
        assert!(err.to_string().contains("api_version"), "{err}");
    }

    #[test]
    fn padded_api_version_is_rejected() {
        // " 2024-10-21" would be percent-encoded verbatim into the request
        // URL (api-version=%202024-10-21) and fail only at request time with
        // an Azure 4xx, so surrounding whitespace is rejected at boot.
        for padded in [" 2024-10-21", "2024-10-21 ", "\t2024-10-21\n"] {
            let toml = format!(
                r#"
                    [[providers]]
                    name = "azure"
                    kind = "azure"
                    base_url = "https://my-resource.openai.azure.com"
                    api_version = {padded:?}
                    [[providers.models]]
                    id = "gpt"
                    capabilities = ["chat"]
                "#
            );
            let err = load_str(&toml).unwrap_err();
            assert!(err.to_string().contains("api_version"), "{err}");
        }
    }

    #[test]
    fn provider_without_api_version_leaves_the_spec_none() {
        let cfg = load_str(VALID).unwrap();
        for spec in cfg.provider_specs() {
            assert_eq!(spec.api_version, None);
        }
    }

    #[test]
    fn provider_without_connect_override_leaves_the_spec_none() {
        let cfg = load_str(VALID).unwrap();
        for spec in cfg.provider_specs() {
            assert_eq!(spec.connect_timeout_ms, None);
        }
    }

    #[test]
    fn tokenizer_mode_defaults_to_heuristic_and_parses_accurate() {
        let cfg = load_str("").unwrap();
        assert_eq!(cfg.tokenizer.mode, TokenizerMode::Heuristic);
        let cfg = load_str("[tokenizer]\nmode = \"accurate\"\n").unwrap();
        assert_eq!(cfg.tokenizer.mode, TokenizerMode::Accurate);
        // An unknown mode is rejected at parse time.
        assert!(matches!(
            load_str("[tokenizer]\nmode = \"exact\"\n").unwrap_err(),
            ConfigError::Parse { .. }
        ));
    }

    #[test]
    fn loaded_models_summary_lists_all_models() {
        let cfg = load_str(VALID).unwrap();
        let models = cfg.loaded_models();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].provider, "openai-main");
        assert!(models.iter().any(|m| m.id == "gpt-4o"));
    }

    // ---- Boot vs dynamic layer split (ADR 012, task 4) ---------------------

    #[test]
    fn boot_layer_diff_names_changed_restart_only_keys() {
        let a = "[server]\nport = 8080\n";
        let b = "[server]\nport = 9090\nhost = \"127.0.0.1\"\n"; // host explicit = default: no diff
        assert_eq!(
            boot_layer_diff(a, b).unwrap(),
            vec!["server.port".to_owned()]
        );
        assert!(boot_layer_diff(a, a).unwrap().is_empty());
        // Dynamic-only change: no boot diff.
        let c = "[server]\nport = 8080\n[tokenizer]\nmode = \"accurate\"\n";
        assert!(boot_layer_diff(a, c).unwrap().is_empty());
    }

    #[test]
    fn ensure_boot_only_rejects_dynamic_keys_and_auth_knobs() {
        assert!(ensure_boot_only("[server]\nport = 1\n[auth]\nenabled = true\n", "t").is_ok());
        let err = ensure_boot_only("[[providers]]\nname = \"x\"\n", "t").unwrap_err();
        assert!(err.to_string().contains("providers"));
        let err = ensure_boot_only("[auth]\nflush_interval_ms = 5\n", "t").unwrap_err();
        assert!(err.to_string().contains("flush_interval_ms"));
    }

    #[test]
    #[allow(clippy::result_large_err)]
    fn load_with_dynamic_merges_boot_file_and_db_text() {
        // figment::Jail as in the existing config tests.
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "boot.toml",
                "config_source = \"db\"\n[server]\nport = 9999\n[auth]\nenabled = true\ndb_path = \"x.db\"\n",
            )?;
            let cfg = Config::load_with_dynamic(
                std::path::Path::new("boot.toml"),
                "[tokenizer]\nmode = \"accurate\"\n",
            )
            .unwrap();
            assert_eq!(cfg.server.port, 9999);
            assert_eq!(cfg.tokenizer.mode, TokenizerMode::Accurate);
            assert_eq!(cfg.config_source, ConfigSourceKind::Db);
            Ok(())
        });
    }

    #[test]
    #[allow(clippy::result_large_err)]
    fn empty_dynamic_document_is_valid() {
        figment::Jail::expect_with(|jail| {
            jail.create_file("boot.toml", "[auth]\nenabled = true\ndb_path = \"x.db\"\n")?;
            let cfg = Config::load_with_dynamic(std::path::Path::new("boot.toml"), "").unwrap();
            assert!(cfg.providers.is_empty());
            Ok(())
        });
    }

    #[test]
    fn every_config_field_is_classified_boot_or_dynamic() {
        // Pin the exhaustive classification of Config's top-level fields, so
        // adding one fails CI until it is classified in BootView,
        // ensure_boot_only AND here.
        //
        // Uses serde_json, not toml: a toml serializer has nowhere to put an
        // `Option::None` value (TOML has no `null`), so `webhooks: None` would
        // be silently dropped and the census would miss it. serde_json keeps
        // every field (`null` included), so the key set below is genuinely
        // exhaustive over `Config`'s fields, not just the ones that happen to
        // serialize as TOML.
        let cfg = Config::default();
        let value = serde_json::to_value(&cfg).unwrap();
        let mut keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "auth",
                "config_source",
                "image_fetch",
                "log_format",
                "providers",
                "resilience",
                "server",
                "telemetry",
                "tokenizer",
                "webhooks",
            ]
        );
    }
}
