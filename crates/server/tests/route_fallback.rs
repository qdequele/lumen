//! Integration tests for the unmatched-route fallback (issue #88).
//!
//! Requests matching no route used to return a bare, empty-body 404 outside
//! the LM error envelope. A `Router::fallback` now answers every such miss
//! with the standard `LM-1003` route-not-found envelope, while the matched
//! operational routes (`/health`, `/metrics`) keep their exact behavior.

mod common;

/// A trailing-slash near-miss (`/health/`) matches no route and must return the
/// `LM-1003` envelope, not a bare 404.
#[tokio::test]
async fn trailing_slash_miss_returns_lm_1003_envelope() {
    let base = common::spawn().await;

    let resp = reqwest::get(format!("{base}/health/")).await.unwrap();
    assert_eq!(resp.status(), 404);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "LM-1003");
    assert_eq!(body["error"]["type"], "invalid_request");
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|m| !m.is_empty()),
        "route-miss envelope must carry a non-empty message, got {body:?}"
    );
}

/// An extra path segment beyond a known route (`/v1/models/extra/segments` is
/// covered by the wildcard, but a wholly unknown prefix is not) must also
/// return the envelope.
#[tokio::test]
async fn extra_segment_miss_returns_lm_1003_envelope() {
    let base = common::spawn().await;

    let resp = reqwest::get(format!("{base}/v1/does/not/exist"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "LM-1003");
    assert_eq!(body["error"]["type"], "invalid_request");
}

/// The fallback response still carries the default security headers and a
/// request id, since it flows through the same middleware stack as every other
/// response.
#[tokio::test]
async fn route_miss_still_carries_request_id_and_security_headers() {
    let base = common::spawn().await;

    let resp = reqwest::get(format!("{base}/nope")).await.unwrap();
    assert_eq!(resp.status(), 404);

    let headers = resp.headers();
    assert_eq!(
        headers
            .get("x-content-type-options")
            .and_then(|v| v.to_str().ok()),
        Some("nosniff")
    );
    assert!(
        headers.get("x-request-id").is_some_and(|v| !v.is_empty()),
        "route-miss response must still carry an x-request-id"
    );
}

/// `/health` latency isolation and body are unchanged by the fallback: a
/// matched route still returns its 200 JSON, never the fallback envelope.
#[tokio::test]
async fn health_behavior_is_unchanged_by_the_fallback() {
    let base = common::spawn().await;

    let resp = reqwest::get(format!("{base}/health")).await.unwrap();
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "ok");
    // The fallback code must never appear on a matched route.
    assert!(body["error"].is_null());
}
