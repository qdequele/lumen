//! Background provider health checks (M6 §6.5).
//!
//! An **optional** (default off) periodic task probes each provider that has a
//! configured `base_url` and records the result in an in-memory registry plus
//! the `lumen_provider_up` gauge. The results are exposed at
//! `/health/providers` purely for observability — the gateway's own `/health`
//! stays independent of provider health, and nothing here is ever consulted on
//! the request path (the executor's live circuit breaker is the request-path
//! signal). Providers that rely on a built-in vendor URL report `unknown`; the
//! gateway never hardcodes vendor endpoints to probe.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::Json;
use dashmap::DashMap;
use lumen_telemetry::ResilienceMetrics;
use serde::Serialize;

use crate::auth::now_unix;
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
    /// The base URL answered (any HTTP status — the host is reachable).
    Up,
    /// The base URL could not be reached (transport error / timeout).
    Down,
    /// Never probed (health checks off, or no configured base URL).
    Unknown,
}

/// In-memory registry of provider health, shared across the probe task and the
/// `/health/providers` handler. Reads are a lock-free map lookup — safe to hit
/// often, and never on the request hot path anyway.
#[derive(Debug, Default)]
pub struct ProviderHealth {
    statuses: DashMap<String, ProviderStatus>,
}

impl ProviderHealth {
    /// Pre-populate every provider as `unknown` so `/health/providers` lists
    /// them all from the first scrape.
    #[must_use]
    pub fn with_providers(names: &[String]) -> Self {
        let statuses = DashMap::new();
        for name in names {
            statuses.insert(name.clone(), ProviderStatus::unknown());
        }
        Self { statuses }
    }

    /// Record a probe result.
    pub fn record(&self, provider: &str, status: ProviderStatus) {
        self.statuses.insert(provider.to_owned(), status);
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

/// `GET /health/providers` — the observability view of background health
/// checks. Always mounted (returns all-`unknown` when checks are disabled);
/// deliberately separate from `/health`, which never depends on provider state.
pub async fn providers_health(
    State(state): State<AppState>,
) -> Json<std::collections::BTreeMap<String, ProviderStatus>> {
    Json(state.health.snapshot())
}

/// A provider to probe: its name, configured base URL, and kind (which decides
/// the liveness endpoint — a real one where the kind exposes it, else the base
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

impl ProbeTarget {
    /// The URL to GET: a real liveness endpoint where the kind has one (TEI
    /// serves `/health`), otherwise the base URL for host reachability.
    fn probe_url(&self) -> String {
        match self.kind {
            lumen_providers::ProviderKind::Tei => {
                format!("{}/health", self.url.trim_end_matches('/'))
            }
            _ => self.url.clone(),
        }
    }

    /// Whether the probe hits a true liveness endpoint (so a non-2xx means the
    /// server is up but *not ready* → down) or just checks reachability (any
    /// response means the host is up).
    fn is_liveness_endpoint(&self) -> bool {
        matches!(self.kind, lumen_providers::ProviderKind::Tei)
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
        if let Some(metrics) = metrics {
            metrics.set_provider_up(&target.name, status.status == HealthState::Up);
        }
        health.record(&target.name, status);
    }
}

/// Spawn the periodic health-check task. No-op-friendly: the caller only calls
/// this when `resilience.health_check_enabled` is set and there is at least one
/// probe target. The task ticks forever; it is aborted when the process exits.
pub fn spawn_health_checks(
    client: reqwest::Client,
    targets: Vec<ProbeTarget>,
    health: Arc<ProviderHealth>,
    metrics: Option<ResilienceMetrics>,
    interval: Duration,
    probe_timeout: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
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
    fn probe_url_uses_a_liveness_endpoint_only_where_the_kind_has_one() {
        use lumen_providers::ProviderKind;
        let tei = ProbeTarget {
            name: "tei".to_owned(),
            url: "http://tei:8080/".to_owned(),
            kind: ProviderKind::Tei,
        };
        assert_eq!(tei.probe_url(), "http://tei:8080/health");
        assert!(tei.is_liveness_endpoint());

        let ollama = ProbeTarget {
            name: "ollama".to_owned(),
            url: "http://ollama:11434".to_owned(),
            kind: ProviderKind::Ollama,
        };
        // No known liveness endpoint → bare reachability against the base URL.
        assert_eq!(ollama.probe_url(), "http://ollama:11434");
        assert!(!ollama.is_liveness_endpoint());
    }

    #[test]
    fn unknown_status_serializes_without_optional_fields() {
        let json = serde_json::to_value(ProviderStatus::unknown()).unwrap();
        assert_eq!(json["status"], "unknown");
        assert!(json.get("checked_at").is_none());
        assert!(json.get("latency_ms").is_none());
    }
}
