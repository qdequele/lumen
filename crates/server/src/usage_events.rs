//! Billing usage-event delivery (ADR 015): pushes due outbox rows to the
//! control plane in signed batches and marks what it acknowledges. Never on
//! the request path; a failing or unreachable control plane only delays
//! delivery. The one drop: an event a reachable control plane kept out of
//! `accepted` for [`REJECTION_TTL_MS`] since its first such answer
//! (contract v2 section 3.5).

use crate::webhooks::SigningKey;
use lumen_auth::store::{KeyStore, OutboxRow};
use lumen_telemetry::UsageEventMetrics;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Header naming the reporting deployment (`LAB_INSTANCE_ID`).
pub const INSTANCE_HEADER: &str = "X-Lab-Instance-Id";
/// Header carrying the unix time (whole seconds) the signature covers.
pub const TIMESTAMP_HEADER: &str = "X-Lab-Timestamp";
/// Header carrying `sha256=<hex HMAC-SHA256(secret, "<timestamp>.<body>")>`.
pub const SIGNATURE_HEADER: &str = "X-Lab-Signature";
/// How often the sender polls the outbox when it is not draining a backlog.
const POLL: Duration = Duration::from_secs(2);
/// The pending and oldest-age gauges are re-read at most this often (ms):
/// a scrape interval is coarser, and a backlog drain loops without waiting.
const GAUGE_REFRESH_MS: i64 = 10_000;
/// Delivered (and dropped) rows are kept this long, then purged.
pub const DELIVERED_RETENTION_MS: i64 = 7 * 86_400_000;
/// A row the Lab has kept out of `accepted` for this long, counted from the
/// first 2xx answer that skipped it, is dropped (contract v2 section 3.5).
/// Only such answers start the clock: an unreachable or failing Lab never
/// drops a row, and a row skipped for the first time is never dropped.
pub const REJECTION_TTL_MS: i64 = 24 * 3_600_000;

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

/// `hex HMAC-SHA256(secret, "<timestamp>.<body>")`: the timestamp binds the
/// signature to a 300 s window on the Lab side (spec section 3.4).
pub(crate) fn sign_batch(key: &SigningKey, timestamp: &str, body: &[u8]) -> String {
    let mut signed = Vec::with_capacity(timestamp.len() + 1 + body.len());
    signed.extend_from_slice(timestamp.as_bytes());
    signed.push(b'.');
    signed.extend_from_slice(body);
    key.sign(&signed)
}

/// Delivers billing usage events from the outbox.
pub struct UsageEventsSender {
    store: KeyStore,
    client: reqwest::Client,
    endpoint: String,
    instance_id: String,
    signing: SigningKey,
    batch_size: i64,
    metrics: UsageEventMetrics,
}

impl fmt::Debug for UsageEventsSender {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UsageEventsSender")
            .field("endpoint", &self.endpoint)
            .field("instance_id", &self.instance_id)
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
        instance_id: String,
        signing: SigningKey,
        batch_size: usize,
        metrics: UsageEventMetrics,
    ) -> Self {
        Self {
            store,
            client,
            endpoint,
            instance_id,
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
                self.metrics.inc_failed("store", 1);
                return Delivery::Failed;
            }
        };
        if rows.is_empty() {
            return Delivery::Idle;
        }
        let ids: Vec<String> = rows.iter().map(|r| r.id.clone()).collect();
        let body = batch_body(&rows);
        let timestamp = now_ms.div_euclid(1000).to_string();
        let signature = format!(
            "sha256={}",
            sign_batch(&self.signing, &timestamp, body.as_bytes())
        );
        let response = self
            .client
            .post(&self.endpoint)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(INSTANCE_HEADER, &self.instance_id)
            .header(TIMESTAMP_HEADER, &timestamp)
            .header(SIGNATURE_HEADER, signature)
            .body(body)
            .send()
            .await;
        // `without_url` strips the endpoint from the transport error; the
        // body and the signing key are never logged.
        let (reason, detail) = match response {
            Err(error) if error.is_timeout() => ("timeout", error.without_url().to_string()),
            Err(error) => ("connect", error.without_url().to_string()),
            Ok(resp) if resp.status() == reqwest::StatusCode::UNAUTHORIZED => {
                ("auth", String::new())
            }
            Ok(resp) if !resp.status().is_success() => {
                ("status", format!("HTTP {}", resp.status().as_u16()))
            }
            Ok(resp) => match resp.json::<Ack>().await {
                Ok(ack) => return self.settle_ack(&rows, &ack.accepted, now_ms).await,
                Err(error) => ("malformed", error.without_url().to_string()),
            },
        };
        if reason == "auth" {
            tracing::error!(endpoint = %self.endpoint, "usage events rejected with 401: check LAB_INSTANCE_ID and the instance secret");
        } else {
            tracing::warn!(endpoint = %self.endpoint, reason, error = %detail, "usage events delivery failed; will retry");
        }
        self.metrics
            .inc_failed(reason, u64::try_from(ids.len()).unwrap_or(0));
        self.reschedule(&ids, now_ms).await;
        Delivery::Failed
    }

    async fn settle_ack(&self, rows: &[OutboxRow], accepted: &[String], now_ms: i64) -> Delivery {
        let (done, rest): (Vec<&OutboxRow>, Vec<&OutboxRow>) =
            rows.iter().partition(|row| accepted.contains(&row.id));
        let done: Vec<String> = done.into_iter().map(|row| row.id.clone()).collect();
        if let Err(error) = self.store.outbox_mark_delivered(&done, now_ms).await {
            // The Lab has them and dedups on id: a re-send is harmless.
            tracing::warn!(%error, "usage events: could not mark delivered rows");
            self.metrics
                .inc_failed("store", u64::try_from(done.len()).unwrap_or(0));
            return Delivery::Failed;
        }
        self.metrics
            .add_delivered(u64::try_from(done.len()).unwrap_or(0));
        if rest.is_empty() {
            return Delivery::Delivered(done.len());
        }
        // The Lab is reachable and skipped these ids: the first skip starts
        // a 24 h clock, and a skip 24 h later is permanent (account_not_owned,
        // an unknown account, ...).
        let rest: Vec<String> = rest.into_iter().map(|row| row.id.clone()).collect();
        let retry: Vec<String> = match self
            .store
            .outbox_record_skips(&rest, now_ms, REJECTION_TTL_MS)
            .await
        {
            Ok(settled) => {
                let mut retry = Vec::with_capacity(settled.len());
                let mut dropped = 0_u64;
                for row in settled {
                    if row.dropped {
                        dropped += 1;
                        tracing::error!(
                            event_id = %row.id,
                            skipped_hours = now_ms.saturating_sub(row.first_skipped_ms) / 3_600_000,
                            "usage event refused by the Lab for 24 h; dropped (see the body in usage_outbox)"
                        );
                    } else {
                        retry.push(row.id);
                    }
                }
                self.metrics.add_dropped(dropped);
                retry
            }
            // Nothing was recorded: no clock started, nothing dropped. The
            // rows are retried and the next skip records them.
            Err(error) => {
                tracing::warn!(%error, "usage events: could not record skipped rows");
                rest
            }
        };
        if !retry.is_empty() {
            tracing::warn!(
                count = retry.len(),
                "usage events not accepted by the control plane; will retry"
            );
            // Only rows that will be retried count here; a dropped row
            // counts in `lumen_usage_events_dropped_total` alone.
            self.metrics
                .inc_failed("not_accepted", u64::try_from(retry.len()).unwrap_or(0));
            self.reschedule(&retry, now_ms).await;
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

    /// Purge delivered and dropped rows past retention.
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
/// waiting, refresh gauges at once and then at most every 10 s, purge hourly.
pub fn spawn_usage_events_sender(
    sender: Arc<UsageEventsSender>,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut last_purge = 0_i64;
        let mut last_gauges = i64::MIN;
        loop {
            let now = lumen_auth::now_unix_ms();
            let outcome = tokio::select! {
                biased;
                () = cancel.cancelled() => break,
                outcome = sender.deliver_once(now) => outcome,
            };
            if now.saturating_sub(last_gauges) >= GAUGE_REFRESH_MS {
                sender.refresh_gauges(now).await;
                last_gauges = now;
            }
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
    use wiremock::matchers::{header, header_exists, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const SECRET: &str = "test-events-secret";
    const INSTANCE: &str = "0192f3c1-7c2e-7b1a-9f00-3c9d2e4a5b71";

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
            INSTANCE.to_owned(),
            SigningKey::new(SECRET.as_bytes().to_vec()),
            500,
            m,
        );
        (s, store, metrics)
    }

    #[tokio::test]
    async fn delivers_a_batch_signed_as_this_instance_and_marks_accepted_rows() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/internal/events"))
            .and(header_exists(SIGNATURE_HEADER))
            .and(header(INSTANCE_HEADER, INSTANCE))
            .and(header(TIMESTAMP_HEADER, "1700000000"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"accepted":["a","b"]})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let (s, store, metrics) = sender(&server, &["a", "b"]).await;
        // 1 700 000 000 s and 123 ms: the timestamp is whole seconds.
        assert_eq!(
            s.deliver_once(1_700_000_000_123).await,
            Delivery::Delivered(2)
        );
        let got = store.outbox_due(i64::MAX, 10).await.unwrap();
        assert!(got.is_empty(), "{got:?}");

        let req = &server.received_requests().await.unwrap()[0];
        let body = std::str::from_utf8(&req.body).unwrap();
        assert_eq!(body, r#"{"events":[{"id":"a"},{"id":"b"}]}"#);
        // sha256=HMAC(secret, "<timestamp>.<body>"), spec section 3.4.
        let mut signed = b"1700000000.".to_vec();
        signed.extend_from_slice(req.body.as_slice());
        let expected = format!(
            "sha256={}",
            SigningKey::new(SECRET.as_bytes().to_vec()).sign(&signed)
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
        // A second server that must never be contacted: wiremock verifies
        // `.expect(0)` when it drops.
        let target = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&target)
            .await;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(307)
                    .insert_header("location", format!("{}/x", target.uri()).as_str()),
            )
            .mount(&server)
            .await;
        let (s, store, metrics) = sender(&server, &["a"]).await;
        assert_eq!(s.deliver_once(10).await, Delivery::Failed);
        assert_eq!(store.outbox_stats().await.unwrap().0, 1);
        assert!(
            metrics
                .encode_text()
                .contains(r#"lumen_usage_events_failed_total{reason="status"} 1"#),
            "a 307 is a status failure; a followed redirect would be a different reason"
        );
    }

    #[tokio::test]
    async fn the_loop_backs_off_on_failure_and_cancels_promptly() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let (s, _, _) = sender(&server, &["a"]).await;
        let cancel = CancellationToken::new();
        let handle = spawn_usage_events_sender(Arc::new(s), cancel.clone());
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("the sender stops promptly on cancel")
            .unwrap();
        let requests = server.received_requests().await.unwrap().len();
        assert!(
            (1..=2).contains(&requests),
            "a failing Lab must not be hot-spun: {requests} requests"
        );
    }

    #[tokio::test]
    async fn the_loop_refreshes_gauges_at_once_then_at_most_every_10s() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let (s, store, metrics) = sender(&server, &["a"]).await;
        let cancel = CancellationToken::new();
        let handle = spawn_usage_events_sender(Arc::new(s), cancel.clone());
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            metrics
                .encode_text()
                .contains("lumen_usage_events_pending 1"),
            "the first refresh is immediate"
        );
        // A second pending row, then one more loop iteration (2 s poll): the
        // gauges are not re-read before 10 s have passed.
        store
            .persist_flush(
                &[],
                &[OutboxInsert {
                    id: "b".to_owned(),
                    body: r#"{"id":"b"}"#.to_owned(),
                    created_ms: 1,
                }],
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(2_500)).await;
        assert!(server.received_requests().await.unwrap().len() >= 2);
        assert!(
            metrics
                .encode_text()
                .contains("lumen_usage_events_pending 1"),
            "no gauge refresh within 10 s of the last one"
        );
        cancel.cancel();
        handle.await.unwrap();
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
    async fn an_event_the_lab_keeps_skipping_is_dropped_24h_after_its_first_skip() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"accepted":[]})),
            )
            .mount(&server)
            .await;
        let (s, store, metrics) = sender(&server, &["a"]).await;
        let t0 = 10;
        // First skip: the clock starts, the row is kept and retried.
        assert_eq!(s.deliver_once(t0).await, Delivery::Delivered(0));
        assert_eq!(store.outbox_stats().await.unwrap().0, 1);
        // 10 s short of 24 h after the first skip: still kept (and backed
        // off by 4 s plus jitter, so due again at the 24 h mark).
        assert_eq!(
            s.deliver_once(t0 + REJECTION_TTL_MS - 10_000).await,
            Delivery::Delivered(0)
        );
        assert_eq!(store.outbox_stats().await.unwrap().0, 1);
        assert!(!metrics
            .encode_text()
            .contains("lumen_usage_events_dropped_total 1"));
        // Exactly 24 h after the first skip: dropped.
        assert_eq!(
            s.deliver_once(t0 + REJECTION_TTL_MS).await,
            Delivery::Delivered(0)
        );
        assert_eq!(store.outbox_stats().await.unwrap().0, 0, "a was dropped");
        assert_eq!(
            store.outbox_due(i64::MAX, 10).await.unwrap(),
            [] as [OutboxRow; 0]
        );
        let text = metrics.encode_text();
        assert!(
            text.contains("lumen_usage_events_dropped_total 1\n"),
            "{text}"
        );
        // The two retried rounds count; the dropping round does not.
        assert!(
            text.contains("lumen_usage_events_failed_total{reason=\"not_accepted\"} 2\n"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn a_lab_back_from_a_long_outage_does_not_drop_on_its_first_skip() {
        const H: i64 = 3_600_000;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"accepted":[]})),
            )
            .mount(&server)
            .await;
        let (s, store, metrics) = sender(&server, &["a"]).await; // created at 0
        assert_eq!(s.deliver_once(0).await, Delivery::Failed);
        // Back after 30 h: the first skip starts the clock, nothing drops.
        assert_eq!(s.deliver_once(30 * H).await, Delivery::Delivered(0));
        assert_eq!(
            store.outbox_stats().await.unwrap().0,
            1,
            "the first skip never drops"
        );
        let due = store.outbox_due(i64::MAX, 10).await.unwrap();
        assert_eq!(due[0].first_skipped_ms, Some(30 * H));
        assert!(!metrics
            .encode_text()
            .contains("lumen_usage_events_dropped_total 1"));
        // Skipped again 24 h later: dropped.
        assert_eq!(s.deliver_once(54 * H).await, Delivery::Delivered(0));
        assert_eq!(store.outbox_stats().await.unwrap().0, 0);
        assert!(metrics
            .encode_text()
            .contains("lumen_usage_events_dropped_total 1\n"));
    }

    #[tokio::test]
    async fn a_lab_that_stays_unreachable_never_drops_a_row() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let (s, store, metrics) = sender(&server, &["a"]).await;
        assert_eq!(
            s.deliver_once(REJECTION_TTL_MS * 30).await,
            Delivery::Failed
        );
        assert_eq!(store.outbox_stats().await.unwrap().0, 1);
        assert!(!metrics
            .encode_text()
            .contains("lumen_usage_events_dropped_total 1"));
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
