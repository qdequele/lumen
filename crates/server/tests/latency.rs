//! End-to-end latency observability tests: every endpoint feeds the
//! `lumen_http_request_duration_seconds` histogram (labelled by matched route,
//! never the raw path), and accounted API calls additionally feed
//! `lumen_request_duration_seconds` with capability/model/provider labels.

mod common;

use std::sync::Arc;
use std::time::Duration;

use lumen_core::Capability;
use lumen_providers::{http, ModelSpec, ProviderKind, ProviderSpec, Registry};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const LIMIT: usize = 10 * 1024 * 1024;

/// One OpenAI-kind chat provider pointed at `upstream`.
fn openai_registry(upstream: &str) -> Arc<Registry> {
    let specs = vec![ProviderSpec {
        name: "openai".to_owned(),
        kind: ProviderKind::Openai,
        api_key: Some("sk-test-xxx".to_owned()),
        base_url: Some(upstream.to_owned()),
        strict: false,
        connect_timeout_ms: None,
        models: vec![ModelSpec {
            id: "gpt".to_owned(),
            upstream_id: "gpt-4o-2024-08-06".to_owned(),
            capabilities: vec![Capability::Chat],
            modalities: vec!["text".to_owned()],
        }],
    }];
    Arc::new(
        Registry::build(
            specs,
            http::build_client(),
            std::time::Duration::from_secs(300),
        )
        .expect("registry builds"),
    )
}

async fn scrape_metrics(base: &str) -> String {
    reqwest::get(format!("{base}/metrics"))
        .await
        .expect("scrape /metrics")
        .text()
        .await
        .expect("metrics body")
}

#[tokio::test]
async fn every_endpoint_feeds_the_http_latency_histogram() {
    let base = common::spawn().await;

    let health = reqwest::get(format!("{base}/health"))
        .await
        .expect("GET /health");
    assert_eq!(health.status(), 200);

    let out = scrape_metrics(&base).await;
    assert!(
        out.contains("lumen_http_request_duration_seconds"),
        "histogram missing from /metrics:\n{out}"
    );
    assert!(
        out.contains(r#"path="/health""#),
        "matched route label missing:\n{out}"
    );
    assert!(out.contains(r#"method="GET""#));
    assert!(out.contains(r#"status="200""#));
}

#[tokio::test]
async fn the_metrics_endpoint_itself_is_measured() {
    let base = common::spawn().await;

    // First scrape observes itself only on the NEXT scrape.
    let _ = scrape_metrics(&base).await;
    let out = scrape_metrics(&base).await;
    assert!(out.contains(r#"path="/metrics""#), "{out}");
}

#[tokio::test]
async fn unmatched_routes_get_a_bounded_label_not_the_raw_path() {
    let base = common::spawn().await;

    let resp = reqwest::get(format!("{base}/nope/secret-looking-path"))
        .await
        .expect("GET unmatched");
    assert_eq!(resp.status(), 404);

    let out = scrape_metrics(&base).await;
    // The raw path must NEVER become a label (cardinality + privacy).
    assert!(!out.contains("secret-looking-path"), "{out}");
    assert!(out.contains(r#"path="unmatched""#), "{out}");
    assert!(out.contains(r#"status="404""#), "{out}");
}

#[tokio::test]
async fn nonstandard_methods_get_a_bounded_label_not_the_raw_string() {
    let base = common::spawn().await;

    let method = reqwest::Method::from_bytes(b"QWERTY").expect("custom method");
    let resp = reqwest::Client::new()
        .request(method, format!("{base}/health"))
        .send()
        .await
        .expect("QWERTY /health");
    assert_eq!(resp.status(), 405);

    let out = scrape_metrics(&base).await;
    // An arbitrary extension method must never mint a new label value.
    assert!(!out.contains("QWERTY"), "{out}");
    assert!(out.contains(r#"method="other""#), "{out}");
}

#[tokio::test]
async fn chat_requests_feed_the_per_model_latency_histogram() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-1",
            "object": "chat.completion",
            "created": 1,
            "model": "gpt-4o-2024-08-06",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "hi"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 3, "completion_tokens": 1, "total_tokens": 4}
        })))
        .mount(&upstream)
        .await;

    let base = common::spawn_with(openai_registry(&upstream.uri()), LIMIT).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({
            "model": "gpt",
            "messages": [{"role": "user", "content": "hello"}]
        }))
        .send()
        .await
        .expect("POST /v1/chat/completions");
    assert_eq!(resp.status(), 200);

    let out = scrape_metrics(&base).await;
    assert!(
        out.contains("lumen_request_duration_seconds"),
        "per-capability histogram missing:\n{out}"
    );
    let line = out
        .lines()
        .find(|l| l.starts_with("lumen_request_duration_seconds_count") && l.contains("chat"))
        .unwrap_or_else(|| panic!("no chat duration sample:\n{out}"));
    assert!(line.contains(r#"capability="chat""#), "{line}");
    assert!(line.contains(r#"model="gpt""#), "{line}");
    assert!(line.contains(r#"provider="openai""#), "{line}");
    assert!(line.contains(r#"status="200""#), "{line}");
    // The HTTP-level histogram sees the same request under its route.
    assert!(out.contains(r#"path="/v1/chat/completions""#), "{out}");
}

/// A raw TCP "upstream" that streams SSE frames forever (no `[DONE]`), so a
/// client disconnect always lands MID-stream. Same shape as
/// `spawn_abort_detecting_upstream` in the chat tests: it stops when it
/// observes the gateway's FIN (a 0-byte read).
async fn spawn_endless_sse_upstream() -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind upstream");
    let addr = listener.local_addr().expect("upstream addr");

    tokio::spawn(async move {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let mut req = [0u8; 4096];
        let _ = socket.read(&mut req).await;
        let (mut rd, mut wr) = socket.split();
        let head =
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";
        if wr.write_all(head.as_bytes()).await.is_err() {
            return;
        }
        let frame = "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"x\"}}]}\n\n";
        let write_loop = async {
            loop {
                if wr.write_all(frame.as_bytes()).await.is_err() {
                    break;
                }
                let _ = wr.flush().await;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        let read_eof = async {
            let mut buf = [0u8; 256];
            loop {
                match rd.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
        };
        tokio::select! {
            () = write_loop => {},
            () = read_eof => {},
        }
    });

    format!("http://{addr}")
}

// Issue #11 acceptance test: a mid-stream client disconnect must be recorded
// as a `status="499"` sample on `lumen_request_duration_seconds` - the
// dedicated client-cancel classification - and never as an internal
// `500`/`5xx` sample. Before the fix this disconnect settled as a fake
// `status="200"` success (StreamAccounting's drop safety net hardcoded 200),
// so the positive `499` assertion below fails on the pre-fix commit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mid_stream_client_disconnect_is_recorded_as_499_not_500_or_200() {
    let upstream_url = spawn_endless_sse_upstream().await;
    let base = common::spawn_with(openai_registry(&upstream_url), LIMIT).await;

    let client = reqwest::Client::new();
    let mut resp = client
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({
            "model": "gpt",
            "messages": [{"role": "user", "content": "hello"}],
            "stream": true
        }))
        .send()
        .await
        .expect("open the stream");
    assert_eq!(resp.status(), 200);

    // Read one streamed frame (the stream is live), then disconnect by
    // dropping the response - a genuine mid-stream client cancel.
    let first = resp.chunk().await.expect("first chunk");
    assert!(first.is_some(), "expected at least one streamed frame");
    drop(resp);

    // The gateway settles accounting when it observes the disconnect (the
    // body stream is dropped asynchronously); poll /metrics until the sample
    // lands rather than sleeping a fixed amount.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let out = loop {
        let out = scrape_metrics(&base).await;
        let recorded = out
            .lines()
            .any(|l| l.starts_with("lumen_request_duration_seconds_count") && l.contains("chat"));
        if recorded || std::time::Instant::now() > deadline {
            break out;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    };

    // Positive: the cancelled stream produced exactly the 499 classification.
    let line = out
        .lines()
        .find(|l| l.starts_with("lumen_request_duration_seconds_count") && l.contains("chat"))
        .unwrap_or_else(|| panic!("no chat duration sample after disconnect:\n{out}"));
    assert!(
        line.contains(r#"status="499""#),
        "a mid-stream client cancel must be recorded as 499, got: {line}"
    );

    // Negative: it must not masquerade as a success or an internal failure.
    assert!(
        !line.contains(r#"status="200""#),
        "client cancel must not be recorded as a 200 success: {line}"
    );
    assert!(
        !out.contains(r#"status="500""#),
        "client cancel must not surface as an internal 500:\n{out}"
    );
    assert!(
        !out.contains(r#"status="5xx""#),
        "client cancel must not surface as a 5xx class label:\n{out}"
    );

    // Server stays responsive afterwards.
    let health = reqwest::get(format!("{base}/health"))
        .await
        .expect("health");
    assert_eq!(health.status(), 200);
}
