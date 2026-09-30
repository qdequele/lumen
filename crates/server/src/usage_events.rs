//! Billing usage-event delivery (ADR 015): pushes due outbox rows to the
//! control plane in signed batches and marks what it acknowledges. Never on
//! the request path; never drops an event; a failing control plane only
//! delays delivery.

use crate::webhooks::SigningKey;
use lumen_auth::store::{KeyStore, OutboxRow};
use lumen_telemetry::UsageEventMetrics;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Header carrying `sha256=<hex HMAC-SHA256 of the raw body>`.
pub const SIGNATURE_HEADER: &str = "X-Lab-Signature";
/// How often the sender polls the outbox when it is not draining a backlog.
const POLL: Duration = Duration::from_secs(2);
/// Delivered rows are kept this long, then purged.
pub const DELIVERED_RETENTION_MS: i64 = 7 * 86_400_000;

/// The outcome of one delivery attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// Nothing was due.
    Idle,
    /// The control plane acknowledged this many events.
    Delivered(usize),
    /// The attempt failed; every row in the batch stays pending.
    Failed,
}

/// The control plane's acknowledgement.
#[derive(serde::Deserialize)]
struct Ack {
    accepted: Vec<String>,
}

/// Delivers billing usage events from the outbox.
pub struct UsageEventsSender {
    store: KeyStore,
    client: reqwest::Client,
    endpoint: String,
    signing: SigningKey,
    batch_size: i64,
    metrics: UsageEventMetrics,
}

impl fmt::Debug for UsageEventsSender {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UsageEventsSender")
            .field("endpoint", &self.endpoint)
            .field("signing", &self.signing)
            .finish_non_exhaustive()
    }
}

impl UsageEventsSender {
    /// Build a sender. `client` must not follow redirects
    /// (`lumen_providers::http::build_client_with`).
    #[must_use]
    pub fn new(
        store: KeyStore,
        client: reqwest::Client,
        endpoint: String,
        signing: SigningKey,
        batch_size: usize,
        metrics: UsageEventMetrics,
    ) -> Self {
        Self {
            store,
            client,
            endpoint,
            signing,
            batch_size: i64::try_from(batch_size).unwrap_or(500),
            metrics,
        }
    }

    /// Send one batch of due events.
    pub async fn deliver_once(&self, now_ms: i64) -> Delivery {
        let rows = match self.store.outbox_due(now_ms, self.batch_size).await {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(%error, "usage events: could not read the outbox");
                return Delivery::Failed;
            }
        };
        if rows.is_empty() {
            return Delivery::Idle;
        }
        let ids: Vec<String> = rows.iter().map(|r| r.id.clone()).collect();
        let body = batch_body(&rows);
        let signature = format!("sha256={}", self.signing.sign(body.as_bytes()));
        let response = self
            .client
            .post(&self.endpoint)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(SIGNATURE_HEADER, signature)
            .body(body)
            .send()
            .await;
        let reason = match response {
            Err(error) if error.is_timeout() => "timeout",
            Err(_) => "connect",
            Ok(resp) if resp.status() == reqwest::StatusCode::UNAUTHORIZED => "auth",
            Ok(resp) if !resp.status().is_success() => "status",
            Ok(resp) => match resp.json::<Ack>().await {
                Ok(ack) => return self.settle_ack(&ids, &ack.accepted, now_ms).await,
                Err(_) => "malformed",
            },
        };
        if reason == "auth" {
            tracing::error!(endpoint = %self.endpoint, "usage events rejected with 401: check the signing secret");
        } else {
            tracing::warn!(endpoint = %self.endpoint, reason, "usage events delivery failed; will retry");
        }
        self.metrics
            .inc_failed(reason, u64::try_from(ids.len()).unwrap_or(0));
        self.reschedule(&ids, now_ms).await;
        Delivery::Failed
    }

    async fn settle_ack(&self, ids: &[String], accepted: &[String], now_ms: i64) -> Delivery {
        let (done, rest): (Vec<String>, Vec<String>) =
            ids.iter().cloned().partition(|id| accepted.contains(id));
        if let Err(error) = self.store.outbox_mark_delivered(&done, now_ms).await {
            // The Lab has them and dedups on id: a re-send is harmless.
            tracing::warn!(%error, "usage events: could not mark delivered rows");
            self.metrics
                .inc_failed("store", u64::try_from(done.len()).unwrap_or(0));
            return Delivery::Failed;
        }
        self.metrics
            .add_delivered(u64::try_from(done.len()).unwrap_or(0));
        if !rest.is_empty() {
            tracing::warn!(
                count = rest.len(),
                "usage events not accepted by the control plane; will retry"
            );
            self.metrics
                .inc_failed("not_accepted", u64::try_from(rest.len()).unwrap_or(0));
            self.reschedule(&rest, now_ms).await;
        }
        Delivery::Delivered(done.len())
    }

    async fn reschedule(&self, ids: &[String], now_ms: i64) {
        if let Err(error) = self.store.outbox_reschedule(ids, now_ms, jitter_ms()).await {
            tracing::warn!(%error, "usage events: could not reschedule failed rows");
        }
    }

    /// Refresh the pending and oldest-age gauges.
    pub async fn refresh_gauges(&self, now_ms: i64) {
        if let Ok((pending, oldest)) = self.store.outbox_stats().await {
            self.metrics.set_pending(pending);
            self.metrics.set_oldest_pending_seconds(
                oldest.map_or(0, |created| (now_ms - created).max(0) / 1000),
            );
        }
    }

    /// Purge delivered rows past retention.
    pub async fn purge(&self, now_ms: i64) {
        if let Err(error) = self
            .store
            .outbox_purge_delivered(now_ms - DELIVERED_RETENTION_MS)
            .await
        {
            tracing::warn!(%error, "usage events: outbox purge failed");
        }
    }
}

/// `{"events":[<stored body>,...]}`: stored bodies are already JSON, so the
/// batch is built by concatenation and sent byte-for-byte as signed.
fn batch_body(rows: &[OutboxRow]) -> String {
    let mut body = String::from(r#"{"events":["#);
    for (i, row) in rows.iter().enumerate() {
        if i > 0 {
            body.push(',');
        }
        body.push_str(&row.body);
    }
    body.push_str("]}");
    body
}

/// 0 to 999 ms of jitter, from the clock's sub-second nanos (not crypto).
fn jitter_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::from(d.subsec_nanos() % 1000))
}

/// Run the sender until `cancel`: poll every 2 s, drain a backlog without
/// waiting, refresh gauges, purge hourly.
pub fn spawn_usage_events_sender(
    sender: Arc<UsageEventsSender>,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut last_purge = 0_i64;
        loop {
            let now = lumen_auth::now_unix_ms();
            let outcome = tokio::select! {
                biased;
                () = cancel.cancelled() => break,
                outcome = sender.deliver_once(now) => outcome,
            };
            sender.refresh_gauges(now).await;
            if now - last_purge > 3_600_000 {
                sender.purge(now).await;
                last_purge = now;
            }
            let backlog = matches!(outcome, Delivery::Delivered(n) if i64::try_from(n).unwrap_or(0) == sender.batch_size);
            if !backlog {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => break,
                    () = tokio::time::sleep(POLL) => {}
                }
            }
        }
        tracing::debug!("usage events sender stopped");
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_auth::store::{KeyStore, OutboxInsert};
    use lumen_telemetry::Metrics;
    use wiremock::matchers::{header_exists, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const SECRET: &str = "test-events-secret";

    async fn sender(server: &MockServer, rows: &[&str]) -> (UsageEventsSender, KeyStore, Metrics) {
        let store = KeyStore::in_memory().await.unwrap();
        let inserts: Vec<OutboxInsert> = rows
            .iter()
            .enumerate()
            .map(|(i, id)| OutboxInsert {
                id: (*id).to_owned(),
                body: format!(r#"{{"id":"{id}"}}"#),
                created_ms: i64::try_from(i).unwrap(),
            })
            .collect();
        store.persist_flush(&[], &inserts).await.unwrap();
        let metrics = Metrics::new();
        let m = UsageEventMetrics::register(&metrics).unwrap();
        let s = UsageEventsSender::new(
            store.clone(),
            lumen_providers::http::build_client_with(
                std::time::Duration::from_secs(2),
                std::time::Duration::from_secs(5),
            ),
            format!("{}/internal/events", server.uri()),
            SigningKey::new(SECRET.as_bytes().to_vec()),
            500,
            m,
        );
        (s, store, metrics)
    }

    #[tokio::test]
    async fn delivers_a_signed_batch_and_marks_accepted_rows() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/internal/events"))
            .and(header_exists(SIGNATURE_HEADER))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"accepted":["a","b"]})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let (s, store, metrics) = sender(&server, &["a", "b"]).await;
        assert_eq!(s.deliver_once(10).await, Delivery::Delivered(2));
        assert!(store.outbox_due(i64::MAX, 10).await.unwrap().is_empty());

        let req = &server.received_requests().await.unwrap()[0];
        let body = std::str::from_utf8(&req.body).unwrap();
        assert_eq!(body, r#"{"events":[{"id":"a"},{"id":"b"}]}"#);
        let expected = format!(
            "sha256={}",
            SigningKey::new(SECRET.as_bytes().to_vec()).sign(req.body.as_slice())
        );
        assert_eq!(
            req.headers.get(SIGNATURE_HEADER).unwrap().to_str().unwrap(),
            expected
        );
        assert!(metrics
            .encode_text()
            .contains("lumen_usage_events_delivered_total 2"));
    }

    #[tokio::test]
    async fn partial_accept_leaves_the_rest_pending() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"accepted":["a"]})),
            )
            .mount(&server)
            .await;
        let (s, store, metrics) = sender(&server, &["a", "b"]).await;
        assert_eq!(s.deliver_once(10).await, Delivery::Delivered(1));
        assert_eq!(store.outbox_stats().await.unwrap().0, 1);
        assert!(metrics
            .encode_text()
            .contains(r#"lumen_usage_events_failed_total{reason="not_accepted"} 1"#));
    }

    #[tokio::test]
    async fn a_5xx_keeps_every_row_and_backs_off() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let (s, store, _) = sender(&server, &["a"]).await;
        assert_eq!(s.deliver_once(10).await, Delivery::Failed);
        assert_eq!(store.outbox_stats().await.unwrap().0, 1);
        assert!(
            store.outbox_due(11, 10).await.unwrap().is_empty(),
            "backed off"
        );
    }

    #[tokio::test]
    async fn unauthorized_is_retried_and_counted() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let (s, store, metrics) = sender(&server, &["a"]).await;
        assert_eq!(s.deliver_once(10).await, Delivery::Failed);
        assert_eq!(store.outbox_stats().await.unwrap().0, 1);
        assert!(metrics
            .encode_text()
            .contains(r#"lumen_usage_events_failed_total{reason="auth"} 1"#));
    }

    #[tokio::test]
    async fn a_malformed_ack_is_a_failure() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .mount(&server)
            .await;
        let (s, store, _) = sender(&server, &["a"]).await;
        assert_eq!(s.deliver_once(10).await, Delivery::Failed);
        assert_eq!(store.outbox_stats().await.unwrap().0, 1);
    }

    #[tokio::test]
    async fn redirects_are_not_followed() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(307).insert_header("location", "https://evil.example/x"),
            )
            .mount(&server)
            .await;
        let (s, store, _) = sender(&server, &["a"]).await;
        assert_eq!(s.deliver_once(10).await, Delivery::Failed);
        assert_eq!(store.outbox_stats().await.unwrap().0, 1);
    }

    #[tokio::test]
    async fn an_empty_outbox_makes_no_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let (s, _, _) = sender(&server, &[]).await;
        assert_eq!(s.deliver_once(10).await, Delivery::Idle);
    }

    #[tokio::test]
    async fn the_secret_never_appears_in_debug() {
        let server = MockServer::start().await;
        let (s, _, _) = sender(&server, &[]).await;
        assert!(!format!("{s:?}").contains(SECRET));
    }

    #[tokio::test]
    async fn gauges_report_pending_and_age() {
        let server = MockServer::start().await;
        let (s, _, metrics) = sender(&server, &["a"]).await; // created_ms = 0
        s.refresh_gauges(90_000).await;
        let text = metrics.encode_text();
        assert!(text.contains("lumen_usage_events_pending 1"));
        assert!(text.contains("lumen_usage_events_oldest_pending_seconds 90"));
    }
}
