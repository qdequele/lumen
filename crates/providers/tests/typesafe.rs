//! TypeSafe SystemOne provider (ADR 013) - wire tests against a mocked
//! `POST /v1/systemone`: verbatim forwarding, bearer auth, usage parsing,
//! status classification (401 / 422 / 429 / 529), malformed bodies,
//! cancellation, and that no secret reaches an error.

use std::time::{Duration, Instant};

use lumen_core::{ProviderError, SystemOneProvider, SystemOneRequest, SystemOneUsage};
use lumen_providers::TypesafeProvider;
use serde_json::json;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const DUMMY_KEY: &str = "ts-test-key-do-not-leak";

/// A documented request whose `state` keys are deliberately NOT sorted, so a
/// re-serialization through `serde_json::Value` would be visible.
const REQUEST: &str = r#"{"model":"jev-latest","state":{"zeta":"Help! My payouts have been failing for 3 days.","alpha":1},"questions":{"is_urgent":{"type":"noul","instructions":"Does this convey urgency?"},"department":{"type":"choice","instructions":"Which team?","criteria":{"technical":"Bugs","billing":"Payments"}}}}"#;

const RESPONSE: &str = r#"{"model":"jev-1.13.0","answers":{"is_urgent":{"type":"noul","noul":0.95},"department":{"type":"choice","choice":"billing","probabilities":{"technical":0.12,"billing":0.88},"confidence":0.81}},"usage":{"input_tokens":318,"output_tokens":34}}"#;

fn request() -> SystemOneRequest {
    serde_json::from_str(REQUEST).expect("valid request")
}

fn provider(uri: String) -> TypesafeProvider {
    TypesafeProvider::new(
        reqwest::Client::new(),
        "typesafe",
        Some(uri),
        Some(DUMMY_KEY.to_owned()),
    )
}

async fn mount(mock: &MockServer, template: ResponseTemplate) {
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(template)
        .mount(mock)
        .await;
}

#[tokio::test]
async fn forwards_the_request_verbatim_with_bearer_auth() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .and(header(
            "authorization",
            format!("Bearer {DUMMY_KEY}").as_str(),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_raw(RESPONSE, "application/json"))
        .expect(1)
        .mount(&mock)
        .await;

    let resp = provider(mock.uri())
        .evaluate(request(), CancellationToken::new())
        .await
        .expect("success");

    assert_eq!(resp.model(), "jev-1.13.0");
    assert_eq!(
        resp.usage,
        Some(SystemOneUsage {
            input_tokens: 318,
            output_tokens: 34,
            estimated: None
        })
    );
    // Answers pass through byte-for-byte (option order included).
    assert!(resp
        .answers()
        .expect("answers")
        .get()
        .contains(r#""probabilities":{"technical":0.12,"billing":0.88}"#));

    // The upstream saw exactly the client's bytes: same key order, no
    // gateway-added fields.
    let received = mock.received_requests().await.expect("recorded");
    assert_eq!(
        std::str::from_utf8(&received[0].body).expect("utf8"),
        REQUEST
    );
}

#[tokio::test]
async fn upstream_model_id_is_what_gets_sent() {
    let mock = MockServer::start().await;
    mount(
        &mock,
        ResponseTemplate::new(200).set_body_raw(RESPONSE, "application/json"),
    )
    .await;
    let mut req = request();
    "jev-1.13.0".clone_into(&mut req.model);
    provider(mock.uri())
        .evaluate(req, CancellationToken::new())
        .await
        .expect("success");
    let received = mock.received_requests().await.expect("recorded");
    let body: serde_json::Value = serde_json::from_slice(&received[0].body).expect("json");
    assert_eq!(body["model"], "jev-1.13.0");
}

#[tokio::test]
async fn missing_usage_is_none_not_zero() {
    let mock = MockServer::start().await;
    mount(
        &mock,
        ResponseTemplate::new(200).set_body_json(json!({"model": "jev-1.13.0", "answers": {}})),
    )
    .await;
    let resp = provider(mock.uri())
        .evaluate(request(), CancellationToken::new())
        .await
        .expect("success");
    assert!(resp.usage.is_none());
}

#[tokio::test]
async fn malformed_usage_does_not_fail_a_billed_answer() {
    // A billed answer must never become a 502 over its usage block: the
    // counts are parsed leniently and the gateway estimates instead.
    for usage in [
        json!({"input_tokens": null}),
        json!({"input_tokens": 318.0}),
        json!("n/a"),
    ] {
        let mock = MockServer::start().await;
        mount(
            &mock,
            ResponseTemplate::new(200)
                .set_body_json(json!({"model": "jev-1.13.0", "answers": {}, "usage": usage})),
        )
        .await;
        let resp = provider(mock.uri())
            .evaluate(request(), CancellationToken::new())
            .await
            .unwrap_or_else(|e| panic!("{usage}: {e:?}"));
        assert!(resp.usage.is_none(), "{usage}");
    }
}

#[tokio::test]
async fn validation_and_auth_errors_are_not_retryable() {
    for status in [401_u16, 422] {
        let mock = MockServer::start().await;
        mount(
            &mock,
            ResponseTemplate::new(status).set_body_json(json!({"detail": "bad"})),
        )
        .await;
        let err = provider(mock.uri())
            .evaluate(request(), CancellationToken::new())
            .await
            .expect_err("upstream error");
        assert!(
            matches!(err, ProviderError::Upstream { status: s, retryable: false, .. } if s == status),
            "{status}: {err:?}"
        );
        assert!(!err.to_string().contains(DUMMY_KEY));
    }
}

#[tokio::test]
async fn overloaded_529_is_retryable() {
    let mock = MockServer::start().await;
    mount(&mock, ResponseTemplate::new(529)).await;
    let err = provider(mock.uri())
        .evaluate(request(), CancellationToken::new())
        .await
        .expect_err("overloaded");
    assert!(
        matches!(
            err,
            ProviderError::Upstream {
                status: 529,
                retryable: true,
                ..
            }
        ),
        "{err:?}"
    );
    assert!(err.is_retryable());
}

#[tokio::test]
async fn rate_limit_carries_retry_after() {
    let mock = MockServer::start().await;
    mount(
        &mock,
        ResponseTemplate::new(429).insert_header("retry-after", "7"),
    )
    .await;
    let err = provider(mock.uri())
        .evaluate(request(), CancellationToken::new())
        .await
        .expect_err("rate limited");
    match err {
        ProviderError::RateLimited { retry_after, .. } => {
            assert_eq!(retry_after, Some(Duration::from_secs(7)));
        }
        other => panic!("expected RateLimited, got {other:?}"),
    }
}

#[tokio::test]
async fn malformed_body_is_a_translation_error() {
    let mock = MockServer::start().await;
    mount(
        &mock,
        ResponseTemplate::new(200).set_body_json(json!({"answers": {}})),
    )
    .await;
    let err = provider(mock.uri())
        .evaluate(request(), CancellationToken::new())
        .await
        .expect_err("no model field");
    assert!(matches!(err, ProviderError::Translation(_)), "{err:?}");
}

#[tokio::test]
async fn cancellation_aborts_the_upstream_call() {
    let mock = MockServer::start().await;
    mount(
        &mock,
        ResponseTemplate::new(200)
            .set_body_raw(RESPONSE, "application/json")
            .set_delay(Duration::from_secs(2)),
    )
    .await;
    let provider = provider(mock.uri());

    let cancel = CancellationToken::new();
    let child = cancel.clone();
    let started = Instant::now();
    let handle = tokio::spawn(async move { provider.evaluate(request(), child).await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    cancel.cancel();

    let result = handle.await.expect("joined");
    assert!(
        matches!(result, Err(ProviderError::Cancelled)),
        "expected Cancelled, got {result:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "cancellation should abort promptly"
    );
    assert_eq!(mock.received_requests().await.expect("recorded").len(), 1);
}
