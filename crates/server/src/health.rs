//! Background provider health checks (M6 §6.5).
//!
//! An **optional** (default off) periodic task probes each provider that has a
//! configured `base_url` and records the result in an in-memory registry plus
//! the `lumen_provider_up` gauge. The results are exposed at
//! `/health/providers` purely for observability - the gateway's own `/health`
//! stays independent of provider health, and nothing here is ever consulted on
//! the request path (the executor's live circuit breaker is the request-path
//! signal). Providers that rely on a built-in vendor URL report `unknown`; the
//! gateway never hardcodes vendor endpoints to probe.
//!
//! The registry follows the live config: every hot reload (including one an
//! admin-API provider edit requests) re-syncs it through
//! [`ProviderHealth::sync_with`], so a provider added at runtime is listed
//! (as `unknown` until probed, then probed like any other) and a removed one
//! disappears.

use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use axum::extract::State;
use axum::Json;
use dashmap::DashMap;
use lumen_telemetry::ResilienceMetrics;
use serde::Serialize;

use crate::auth::now_unix;
use crate::config::Config;
use crate::state::AppState;

/// One provider's last observed health.
#[derive(Debug, Clone, Serialize)]
pub struct ProviderStatus {
    /// `up`, `down`, or `unknown` (never probed / no base URL).
    pub status: HealthState,
    /// Unix seconds of the last probe, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<i64>,
    /// Round-trip latency of the last successful probe, ms.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
    /// A short, secret-free detail (e.g. `"reachable"`, `"unreachable"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl ProviderStatus {
    fn unknown() -> Self {
        Self {
            status: HealthState::Unknown,
            checked_at: None,
            latency_ms: None,
            detail: None,
        }
    }
}

/// Coarse provider reachability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthState {
    /// The base URL answered (any HTTP status - the host is reachable).
    Up,
    /// The base URL could not be reached (transport error / timeout).
    Down,
    /// Never probed (health checks off, or no configured base URL).
    Unknown,
}

/// In-memory registry of provider health, shared across the probe task, the
/// config reloader and the `/health/providers` handler. Reads are a lock-free
/// map lookup - safe to hit often, and never on the request hot path anyway.
#[derive(Debug, Default)]
pub struct ProviderHealth {
    statuses: DashMap<String, ProviderStatus>,
    /// What the probe task checks on its next tick; swapped on reload.
    targets: ArcSwap<Vec<ProbeTarget>>,
    /// The `lumen_provider_up` gauge, once the probe task runs: a provider
    /// dropped by a reload loses its series too.
    gauge: std::sync::OnceLock<ResilienceMetrics>,
}

impl ProviderHealth {
    /// Pre-populate every provider as `unknown` so `/health/providers` lists
    /// them all from the first scrape. No probe targets.
    #[must_use]
    pub fn with_providers(names: &[String]) -> Self {
        let statuses = DashMap::new();
        for name in names {
            statuses.insert(name.clone(), ProviderStatus::unknown());
        }
        Self {
            statuses,
            targets: ArcSwap::default(),
            gauge: std::sync::OnceLock::new(),
        }
    }

    /// Seed from a loaded config: every provider `unknown`, and every
    /// provider with a configured `base_url` as a probe target.
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        let health = Self::default();
        health.sync_with(config);
        health
    }

    /// Re-sync with a (re)loaded config: providers new to the config are
    /// listed as `unknown`, removed ones are dropped, and the ones that stay
    /// keep their last observed status. The probe targets are replaced, so
    /// the next probe round covers exactly the current providers.
    pub fn sync_with(&self, config: &Config) {
        let names: std::collections::HashSet<&str> =
            config.providers.iter().map(|p| p.name.as_str()).collect();
        self.statuses.retain(|name, _| {
            let keep = names.contains(name.as_str());
            if !keep {
                if let Some(gauge) = self.gauge.get() {
                    gauge.remove_provider_up(name);
                }
            }
            keep
        });
        for name in names {
            if !self.statuses.contains_key(name) {
                self.statuses
                    .insert(name.to_owned(), ProviderStatus::unknown());
            }
        }
        self.targets.store(Arc::new(probe_targets(config)));
    }

    /// The providers the probe task should check now.
    #[must_use]
    pub fn targets(&self) -> Arc<Vec<ProbeTarget>> {
        self.targets.load_full()
    }

    /// Record a probe result for a provider the registry lists, returning
    /// whether it was recorded. A result for a provider removed while its
    /// probe was in flight is discarded (`false`), so it cannot reappear after
    /// the reload that dropped it.
    pub fn record(&self, provider: &str, status: ProviderStatus) -> bool {
        self.record_with_gauge(provider, status, None)
    }

    /// [`record`](Self::record), also setting `lumen_provider_up` when the
    /// result is recorded. The gauge is written while the entry guard is
    /// held: `sync_with`'s `retain` takes the same shard write lock, so a
    /// reload cannot drop the provider (and its series) between the status
    /// write and the gauge write, which would recreate the series for a
    /// provider no longer in the config.
    fn record_with_gauge(
        &self,
        provider: &str,
        status: ProviderStatus,
        gauge: Option<&ResilienceMetrics>,
    ) -> bool {
        let Some(mut entry) = self.statuses.get_mut(provider) else {
            return false;
        };
        let up = status.status == HealthState::Up;
        *entry = status;
        if let Some(gauge) = gauge {
            gauge.set_provider_up(provider, up);
        }
        true
    }

    /// A snapshot of every provider's status, provider name → status.
    #[must_use]
    pub fn snapshot(&self) -> std::collections::BTreeMap<String, ProviderStatus> {
        self.statuses
            .iter()
            .map(|e| (e.key().clone(), e.value().clone()))
            .collect()
    }
}

/// `GET /health/providers` - the observability view of background health
/// checks. Always mounted (returns all-`unknown` when checks are disabled);
/// deliberately separate from `/health`, which never depends on provider state.
pub async fn providers_health(
    State(state): State<AppState>,
) -> Json<std::collections::BTreeMap<String, ProviderStatus>> {
    Json(state.health.snapshot())
}

/// A provider to probe: its name, configured base URL, and kind (which decides
/// the liveness endpoint - a real one where the kind exposes it, else the base
/// URL for bare reachability).
#[derive(Debug, Clone)]
pub struct ProbeTarget {
    /// Provider instance name.
    pub name: String,
    /// The base URL (only providers with a configured URL are probed).
    pub url: String,
    /// The provider kind, selecting the liveness endpoint.
    pub kind: lumen_providers::ProviderKind,
}

/// Every provider in `config` with a configured `base_url`: the only ones
/// probed (vendor-default URLs stay `unknown`).
#[must_use]
pub fn probe_targets(config: &Config) -> Vec<ProbeTarget> {
    config
        .providers
        .iter()
        .filter_map(|p| {
            p.base_url.clone().map(|url| ProbeTarget {
                name: p.name.clone(),
                url,
                kind: p.kind,
            })
        })
        .collect()
}

impl ProbeTarget {
    /// The URL to GET: a real liveness endpoint where the kind exposes a
    /// cheap, *unauthenticated* one, otherwise the base URL for bare host
    /// reachability.
    ///
    /// Only self-hosted/keyless kinds get a real liveness endpoint here: TEI
    /// and vLLM both serve `/health` (built for container liveness probes),
    /// and Ollama serves `/api/version`. Keyed vendor kinds (OpenAI,
    /// Anthropic, the OpenAI-compatible hosts, ...) have no endpoint that is
    /// both reliable *and* unauthenticated - hitting `/models` without the
    /// configured API key would 401 a perfectly healthy server, which is a
    /// worse signal than bare reachability. That is deliberately left as
    /// future work (see `docs/backlog.md`); the `_` arm below is also the
    /// integration point for any new `ProviderKind` - it inherits bare
    /// reachability until it earns a real probe.
    fn probe_url(&self) -> String {
        match self.kind {
            lumen_providers::ProviderKind::Tei => {
                format!("{}/health", self.url.trim_end_matches('/'))
            }
            // vLLM serves /health at the SERVER ROOT, but the documented
            // base_url convention for OpenAI-compatible kinds carries a /v1
            // suffix (the chat/embeddings paths are built as {base}/chat/...).
            // Strip that suffix so a healthy server is not probed at the
            // nonexistent /v1/health and wrongly marked down.
            lumen_providers::ProviderKind::Vllm => {
                let base = self.url.trim_end_matches('/');
                let root = base.strip_suffix("/v1").unwrap_or(base);
                format!("{root}/health")
            }
            // Ollama's documented base_url is the server root (no /v1), and
            // /api/version hangs directly off it.
            lumen_providers::ProviderKind::Ollama => {
                format!("{}/api/version", self.url.trim_end_matches('/'))
            }
            _ => self.url.clone(),
        }
    }

    /// Whether the probe hits a true liveness endpoint (so a non-2xx means the
    /// server is up but *not ready* → down) or just checks reachability (any
    /// response means the host is up).
    fn is_liveness_endpoint(&self) -> bool {
        matches!(
            self.kind,
            lumen_providers::ProviderKind::Tei
                | lumen_providers::ProviderKind::Vllm
                | lumen_providers::ProviderKind::Ollama
        )
    }
}

/// Run one round of probes, updating the registry and the gauge. A GET that
/// gets any HTTP response means the host is reachable (`up`); a transport error
/// or timeout means `down`. Never panics; never touches the request path.
pub async fn probe_once(
    client: &reqwest::Client,
    targets: &[ProbeTarget],
    health: &ProviderHealth,
    metrics: Option<&ResilienceMetrics>,
    timeout: Duration,
) {
    for target in targets {
        let started = tokio::time::Instant::now();
        let outcome = client.get(target.probe_url()).timeout(timeout).send().await;
        let elapsed = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let status = match outcome {
            // A liveness endpoint that answers non-2xx means the server is up
            // but not ready → down; a reachability probe treats any response as
            // up (the host answered).
            Ok(response) if target.is_liveness_endpoint() && !response.status().is_success() => {
                ProviderStatus {
                    status: HealthState::Down,
                    checked_at: Some(now_unix()),
                    latency_ms: Some(elapsed),
                    detail: Some(format!(
                        "liveness returned HTTP {}",
                        response.status().as_u16()
                    )),
                }
            }
            Ok(_) => ProviderStatus {
                status: HealthState::Up,
                checked_at: Some(now_unix()),
                latency_ms: Some(elapsed),
                detail: Some(if target.is_liveness_endpoint() {
                    "liveness ok".to_owned()
                } else {
                    "reachable".to_owned()
                }),
            },
            Err(_) => ProviderStatus {
                status: HealthState::Down,
                checked_at: Some(now_unix()),
                latency_ms: None,
                detail: Some("unreachable".to_owned()),
            },
        };
        health.record_with_gauge(&target.name, status, metrics);
    }
}

/// Spawn the periodic health-check task. The caller only calls this when
/// `resilience.health_check_enabled` is set. Each tick probes the registry's
/// *current* targets ([`ProviderHealth::targets`]), so providers added or
/// removed by a reload are picked up without restarting the task; a tick
/// with no target does nothing. The task ticks forever; it is aborted when
/// the process exits.
pub fn spawn_health_checks(
    client: reqwest::Client,
    health: Arc<ProviderHealth>,
    metrics: Option<ResilienceMetrics>,
    interval: Duration,
    probe_timeout: Duration,
) -> tokio::task::JoinHandle<()> {
    if let Some(metrics) = &metrics {
        let _ = health.gauge.set(metrics.clone());
    }
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let targets = health.targets();
            probe_once(&client, &targets, &health, metrics.as_ref(), probe_timeout).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_starts_all_unknown_and_records_updates() {
        let health = ProviderHealth::with_providers(&["a".to_owned(), "b".to_owned()]);
        let snap = health.snapshot();
        assert_eq!(snap.len(), 2);
        assert_eq!(snap["a"].status, HealthState::Unknown);

        health.record(
            "a",
            ProviderStatus {
                status: HealthState::Up,
                checked_at: Some(100),
                latency_ms: Some(7),
                detail: Some("reachable".to_owned()),
            },
        );
        let snap = health.snapshot();
        assert_eq!(snap["a"].status, HealthState::Up);
        assert_eq!(snap["a"].latency_ms, Some(7));
        // Untouched provider stays unknown.
        assert_eq!(snap["b"].status, HealthState::Unknown);
    }

    #[test]
    fn sync_with_follows_the_config_and_keeps_known_statuses() {
        let load = |toml: &str| Config::load_text(toml, "test").expect("config");
        let boot = load(
            r#"
            [[providers]]
            name = "a"
            kind = "openai"
            [[providers]]
            name = "b"
            kind = "tei"
            base_url = "http://tei:8080"
            "#,
        );
        let health = ProviderHealth::from_config(&boot);
        assert_eq!(
            health.snapshot().keys().collect::<Vec<_>>(),
            ["a", "b"],
            "every provider is listed from boot"
        );
        assert_eq!(
            health.targets().len(),
            1,
            "only base_url providers are probed"
        );
        health.record(
            "b",
            ProviderStatus {
                status: HealthState::Up,
                checked_at: Some(1),
                latency_ms: Some(2),
                detail: None,
            },
        );

        // A reload drops `a`, keeps `b` (and its status), and adds `c`.
        let reloaded = load(
            r#"
            [[providers]]
            name = "b"
            kind = "tei"
            base_url = "http://tei:8080"
            [[providers]]
            name = "c"
            kind = "ollama"
            base_url = "http://ollama:11434"
            "#,
        );
        health.sync_with(&reloaded);
        let snap = health.snapshot();
        assert_eq!(snap.keys().collect::<Vec<_>>(), ["b", "c"]);
        assert_eq!(
            snap["b"].status,
            HealthState::Up,
            "a kept provider keeps its status"
        );
        assert_eq!(snap["c"].status, HealthState::Unknown);
        let probed: Vec<String> = health.targets().iter().map(|t| t.name.clone()).collect();
        assert_eq!(probed, ["b", "c"], "the new provider is probed too");

        // A late result for the removed provider does not resurrect it.
        health.record("a", ProviderStatus::unknown());
        assert!(!health.snapshot().contains_key("a"));
    }

    #[test]
    fn probe_url_uses_a_liveness_endpoint_only_where_the_kind_has_one() {
        use lumen_providers::ProviderKind;
        let tei = ProbeTarget {
            name: "tei".to_owned(),
            url: "http://tei:8080/".to_owned(),
            kind: ProviderKind::Tei,
        };
        assert_eq!(tei.probe_url(), "http://tei:8080/health");
        assert!(tei.is_liveness_endpoint());

        // vLLM's documented base_url shape carries the OpenAI-compatible /v1
        // prefix (docs/providers.md), but the server exposes /health at the
        // SERVER ROOT - the probe must strip the /v1 segment, or a healthy
        // vLLM would 404 and be marked down.
        let vllm = ProbeTarget {
            name: "vllm".to_owned(),
            url: "http://vllm:8000/v1".to_owned(),
            kind: ProviderKind::Vllm,
        };
        assert_eq!(vllm.probe_url(), "http://vllm:8000/health");
        assert!(vllm.is_liveness_endpoint());

        // A base_url without the /v1 suffix (or with a trailing slash) still
        // lands on root /health.
        let vllm_root = ProbeTarget {
            name: "vllm-root".to_owned(),
            url: "http://vllm:8000/".to_owned(),
            kind: ProviderKind::Vllm,
        };
        assert_eq!(vllm_root.probe_url(), "http://vllm:8000/health");
        let vllm_v1_slash = ProbeTarget {
            name: "vllm-v1-slash".to_owned(),
            url: "http://vllm:8000/v1/".to_owned(),
            kind: ProviderKind::Vllm,
        };
        assert_eq!(vllm_v1_slash.probe_url(), "http://vllm:8000/health");

        let ollama = ProbeTarget {
            name: "ollama".to_owned(),
            url: "http://ollama:11434".to_owned(),
            kind: ProviderKind::Ollama,
        };
        // Ollama's daemon exposes an unauthenticated, cheap /api/version.
        assert_eq!(ollama.probe_url(), "http://ollama:11434/api/version");
        assert!(ollama.is_liveness_endpoint());

        let openai = ProbeTarget {
            name: "openai".to_owned(),
            url: "https://api.openai.com/v1".to_owned(),
            kind: ProviderKind::Openai,
        };
        // Vendor kinds that require an API key have no reliable *unauthenticated*
        // liveness endpoint (hitting `/models` without a key would 401 a healthy
        // server) → keep the bare-reachability fallback. This is the default
        // arm every new `ProviderKind` inherits until it earns a real probe.
        assert_eq!(openai.probe_url(), "https://api.openai.com/v1");
        assert!(!openai.is_liveness_endpoint());
    }

    #[test]
    fn unknown_status_serializes_without_optional_fields() {
        let json = serde_json::to_value(ProviderStatus::unknown()).unwrap();
        assert_eq!(json["status"], "unknown");
        assert!(json.get("checked_at").is_none());
        assert!(json.get("latency_ms").is_none());
    }

    // -- probe_once against real wiremock upstreams --------------------------
    //
    // These prove the down/up decision, not just URL construction: a kind with
    // a true liveness endpoint must go `down` on a non-2xx from that endpoint,
    // while a bare-reachability kind must stay `up` even on a 404 (any response
    // proves the host is reachable).

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn liveness_endpoint_non_2xx_marks_the_provider_down() {
        use lumen_providers::ProviderKind;

        let upstream = MockServer::start().await;
        // vLLM serves /health at the server ROOT; the documented base_url
        // carries /v1. Mount only root /health so a probe that wrongly hits
        // /v1/health would get wiremock's default 404 instead.
        Mock::given(method("GET"))
            .and(path("/health"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&upstream)
            .await;

        let targets = vec![ProbeTarget {
            name: "vllm".to_owned(),
            url: format!("{}/v1", upstream.uri()),
            kind: ProviderKind::Vllm,
        }];
        let health = ProviderHealth::with_providers(&["vllm".to_owned()]);
        probe_once(
            &reqwest::Client::new(),
            &targets,
            &health,
            None,
            Duration::from_secs(5),
        )
        .await;

        let snap = health.snapshot();
        assert_eq!(snap["vllm"].status, HealthState::Down);
        assert_eq!(
            snap["vllm"].detail.as_deref(),
            Some("liveness returned HTTP 503")
        );
    }

    #[tokio::test]
    async fn healthy_vllm_with_v1_base_url_is_marked_up() {
        use lumen_providers::ProviderKind;

        // Regression guard: with the documented `base_url = ".../v1"` shape,
        // a probe that naively appends /health would hit /v1/health, 404, and
        // mark a HEALTHY vLLM down. Only root /health answers 200 here; any
        // other path gets wiremock's default 404 (a liveness non-2xx = down),
        // so this test fails unless the probe strips the /v1 segment.
        let upstream = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/health"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&upstream)
            .await;

        let targets = vec![ProbeTarget {
            name: "vllm".to_owned(),
            url: format!("{}/v1", upstream.uri()),
            kind: ProviderKind::Vllm,
        }];
        let health = ProviderHealth::with_providers(&["vllm".to_owned()]);
        probe_once(
            &reqwest::Client::new(),
            &targets,
            &health,
            None,
            Duration::from_secs(5),
        )
        .await;

        let snap = health.snapshot();
        assert_eq!(snap["vllm"].status, HealthState::Up);
        assert_eq!(snap["vllm"].detail.as_deref(), Some("liveness ok"));
    }

    #[tokio::test]
    async fn ollama_liveness_endpoint_non_2xx_marks_it_down() {
        use lumen_providers::ProviderKind;

        let upstream = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/version"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&upstream)
            .await;

        let targets = vec![ProbeTarget {
            name: "ollama".to_owned(),
            url: upstream.uri(),
            kind: ProviderKind::Ollama,
        }];
        let health = ProviderHealth::with_providers(&["ollama".to_owned()]);
        probe_once(
            &reqwest::Client::new(),
            &targets,
            &health,
            None,
            Duration::from_secs(5),
        )
        .await;

        assert_eq!(health.snapshot()["ollama"].status, HealthState::Down);
    }

    #[tokio::test]
    async fn ollama_liveness_endpoint_2xx_marks_it_up() {
        use lumen_providers::ProviderKind;

        let upstream = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/version"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "version": "0.5.1"
            })))
            .mount(&upstream)
            .await;

        let targets = vec![ProbeTarget {
            name: "ollama".to_owned(),
            url: upstream.uri(),
            kind: ProviderKind::Ollama,
        }];
        let health = ProviderHealth::with_providers(&["ollama".to_owned()]);
        probe_once(
            &reqwest::Client::new(),
            &targets,
            &health,
            None,
            Duration::from_secs(5),
        )
        .await;

        let snap = health.snapshot();
        assert_eq!(snap["ollama"].status, HealthState::Up);
        assert_eq!(snap["ollama"].detail.as_deref(), Some("liveness ok"));
    }

    #[tokio::test]
    async fn a_provider_dropped_by_a_reload_loses_its_gauge_series() {
        let upstream = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/health"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&upstream)
            .await;
        let with_tei = format!(
            "[[providers]]\nname = \"tei\"\nkind = \"tei\"\nbase_url = \"{}\"\n",
            upstream.uri()
        );
        let registry = lumen_telemetry::Metrics::new();
        let gauge = ResilienceMetrics::register(&registry).expect("register");
        let health = ProviderHealth::from_config(&Config::load_text(&with_tei, "t").expect("cfg"));
        health.gauge.set(gauge.clone()).expect("unset");

        probe_once(
            &reqwest::Client::new(),
            &health.targets(),
            &health,
            Some(&gauge),
            Duration::from_secs(5),
        )
        .await;
        assert!(registry
            .encode_text()
            .contains("lumen_provider_up{provider=\"tei\"} 1"));

        health.sync_with(&Config::load_text("", "t").expect("empty cfg"));
        assert!(!registry.encode_text().contains("provider=\"tei\""));
        // A probe that was still in flight for it does not bring it back.
        probe_once(
            &reqwest::Client::new(),
            &[ProbeTarget {
                name: "tei".to_owned(),
                url: upstream.uri(),
                kind: lumen_providers::ProviderKind::Tei,
            }],
            &health,
            Some(&gauge),
            Duration::from_secs(5),
        )
        .await;
        assert!(!registry.encode_text().contains("provider=\"tei\""));
        assert!(health.snapshot().is_empty());
    }

    #[tokio::test]
    async fn bare_reachability_kind_stays_up_on_a_404() {
        use lumen_providers::ProviderKind;

        // A kind with no per-kind liveness endpoint (e.g. a keyed vendor API)
        // probes the bare base URL; even a 404 there proves the host answered.
        let upstream = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&upstream)
            .await;

        let targets = vec![ProbeTarget {
            name: "openai".to_owned(),
            url: upstream.uri(),
            kind: ProviderKind::Openai,
        }];
        let health = ProviderHealth::with_providers(&["openai".to_owned()]);
        probe_once(
            &reqwest::Client::new(),
            &targets,
            &health,
            None,
            Duration::from_secs(5),
        )
        .await;

        let snap = health.snapshot();
        assert_eq!(snap["openai"].status, HealthState::Up);
        assert_eq!(snap["openai"].detail.as_deref(), Some("reachable"));
    }
}
