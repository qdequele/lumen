//! HTTP-level integration tests for the operational routes.

mod common;

#[tokio::test]
async fn health_returns_ok_json_even_without_any_api_keys() {
    // This test sets NO provider key env vars - /health must still answer 200
    // (acceptance criterion 2: readiness never depends on secrets or I/O).
    let base = common::spawn().await;

    let resp = reqwest::get(format!("{base}/health")).await.unwrap();
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "ok");
}

#[tokio::test]
async fn health_reports_the_gateway_version() {
    // A control plane (the Lab) shows the instance version from /health, so
    // it must be there without credentials, next to the unchanged `status`.
    let base = common::spawn().await;

    let resp = reqwest::get(format!("{base}/health")).await.unwrap();
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "ok");
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
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
    // (32), not some other value `AppState.body_limit` happened to default
    // to - this is the single-source-of-truth invariant `spawn_state`
    // enforces.
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

/// A valid JSON body of about `filler` bytes for each `/v1` POST route. The
/// model is unknown to the (empty) registry, so a body that was accepted and
/// parsed answers `404 LM-2001`, never a body-size or parse error.
fn sized_bodies(filler: usize) -> [(&'static str, String); 4] {
    let big = "x".repeat(filler);
    [
        (
            "/v1/chat/completions",
            format!(r#"{{"model":"nope","messages":[{{"role":"user","content":"{big}"}}]}}"#),
        ),
        (
            "/v1/embeddings",
            format!(r#"{{"model":"nope","input":"{big}"}}"#),
        ),
        (
            "/v1/rerank",
            format!(r#"{{"model":"nope","query":"q","documents":["{big}"]}}"#),
        ),
        (
            "/v1/decisions",
            format!(
                r#"{{"model":"nope","input":"{big}","questions":[{{"type":"predicate","instructions":"?"}}]}}"#
            ),
        ),
    ]
}

/// `server.body_limit` is the only body limit: a body larger than axum's
/// built-in 2 MiB extractor default but under the configured limit is read
/// and parsed on every `/v1` POST route.
#[tokio::test]
async fn a_body_between_2_mib_and_the_limit_is_accepted() {
    let base = common::spawn_with_limit(8 * 1024 * 1024).await;
    let client = reqwest::Client::new();
    for (route, body) in sized_bodies(3 * 1024 * 1024) {
        let resp = client
            .post(format!("{base}{route}"))
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .unwrap();
        let status = resp.status();
        let json: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(status, 404, "{route}: {json}");
        assert_eq!(json["error"]["code"], "LM-2001", "{route}: {json}");
    }
}

/// A chunked body (no `Content-Length`) is only found over the limit while
/// the handler reads it: that is still `413 LM-1002` quoting the configured
/// limit, never a `400 LM-1001`.
#[tokio::test]
async fn an_over_limit_chunked_body_is_a_413_lm_1002() {
    let base = common::spawn_with_limit(1024).await;
    let client = reqwest::Client::new();
    for (route, body) in sized_bodies(64 * 1024) {
        let chunks: Vec<Result<Vec<u8>, std::io::Error>> = body
            .as_bytes()
            .chunks(512)
            .map(|c| Ok(c.to_vec()))
            .collect();
        let resp = client
            .post(format!("{base}{route}"))
            .header("content-type", "application/json")
            .body(reqwest::Body::wrap_stream(futures::stream::iter(chunks)))
            .send()
            .await
            .unwrap();
        let status = resp.status();
        let json: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(status, 413, "{route}: {json}");
        assert_eq!(json["error"]["code"], "LM-1002", "{route}: {json}");
        assert!(
            json["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("1024"),
            "{route}: {json}"
        );
    }
}
