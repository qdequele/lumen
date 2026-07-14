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
async fn responses_carry_default_security_headers() {
    let base = common::spawn().await;
    let resp = reqwest::get(format!("{base}/health")).await.unwrap();
    let headers = resp.headers();
    let get = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    assert_eq!(get("x-content-type-options"), Some("nosniff"));
    assert_eq!(get("x-frame-options"), Some("DENY"));
    assert_eq!(get("referrer-policy"), Some("no-referrer"));
    assert!(get("content-security-policy").is_some_and(|v| v.starts_with("default-src 'none'")));
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
    // The `LM-1002` envelope must advertise the *actual* configured limit
    // (32), not some other value `AppState.body_limit` happened to default to
    // — this is the single-source-of-truth invariant `spawn_state` enforces.
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "LM-1002");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("32"),
        "expected the message to cite the configured 32-byte limit, got {body:?}"
    );
}
