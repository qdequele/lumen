//! HTTP-level integration tests for the operational routes.

mod common;

#[tokio::test]
async fn health_returns_ok_json_even_without_any_api_keys() {
    // This test sets NO provider key env vars — /health must still answer 200
    // (acceptance criterion 2: readiness never depends on secrets or I/O).
    let base = common::spawn().await;

    let resp = reqwest::get(format!("{base}/health")).await.unwrap();
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "ok");
}

#[tokio::test]
async fn every_response_carries_a_request_id() {
    let base = common::spawn().await;

    let resp = reqwest::get(format!("{base}/health")).await.unwrap();
    let request_id = resp
        .headers()
        .get("x-request-id")
        .expect("x-request-id header present");
    assert!(!request_id.is_empty());
}

#[tokio::test]
async fn metrics_returns_prometheus_text_content_type() {
    let base = common::spawn().await;

    let resp = reqwest::get(format!("{base}/metrics")).await.unwrap();
    assert_eq!(resp.status(), 200);

    let content_type = resp
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    assert!(
        content_type.contains("text/plain"),
        "unexpected content-type: {content_type}"
    );

    // An empty registry yields an empty (but valid) exposition body.
    let _body = resp.text().await.unwrap();
}

#[tokio::test]
async fn oversized_body_is_rejected_with_413() {
    // Tiny limit so a modest body trips it.
    let base = common::spawn_with_limit(32).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base}/health"))
        .body("x".repeat(1_000))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 413, "expected Payload Too Large");
}
