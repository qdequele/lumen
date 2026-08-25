//! Streaming time-to-first-byte micro-benchmark.
//!
//! Times the span from dispatching a streaming chat request to reading the
//! first bytes of the SSE body, over real sockets, in two configurations:
//!
//! - `direct_to_upstream`: reqwest client -> mock upstream (the baseline:
//!   loopback hop + client + mock overhead, no gateway involved);
//! - `via_gateway`: reqwest client -> full LUMEN app (axum -> router ->
//!   OpenAI-kind provider) -> the same mock upstream.
//!
//! The difference between the two distributions is the gateway's added time
//! to first bit on the streaming path: how much later a client sees the first
//! streamed token because LUMEN sits in the middle. The upstream answers
//! instantly, so absolute numbers are loopback-dominated; the direct-vs-via
//! *difference* is the meaningful figure (same isolation logic as `bench/`).
//!
//! `tests/chat.rs::first_stream_chunk_reaches_the_client_before_the_upstream_finishes`
//! pins the property this bench quantifies: the first frame is forwarded
//! eagerly, not buffered until end-of-stream. The k6 harness (`bench/`)
//! reports the analogous end-to-end TTFB (`http_req_waiting`) for the
//! non-streaming head-to-head; stock k6 cannot timestamp SSE body chunks, so
//! the streaming variant lives here.
//!
//! Run with `cargo bench -p server --bench stream_ttfb`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use criterion::{criterion_group, criterion_main, Criterion};
use lumen_core::Capability;
use lumen_providers::{http, ModelSpec, ProviderKind, ProviderSpec, Registry};
use lumen_server::{build_app, serve, AppState};
use lumen_telemetry::{LatencyMetrics, Metrics, TokenMetrics};
use serde_json::json;
use tokio::net::TcpListener;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// One OpenAI-kind chat provider pointed at `upstream` (same shape as the
/// integration tests' registry helper).
fn openai_registry(upstream: &str) -> Arc<Registry> {
    let specs = vec![ProviderSpec {
        name: "openai".to_owned(),
        kind: ProviderKind::Openai,
        api_key: Some("sk-bench-xxx".to_owned()),
        base_url: Some(upstream.to_owned()),
        api_version: None,
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
        Registry::build(specs, http::build_client(), Duration::from_secs(300))
            .expect("registry builds"),
    )
}

/// A short upstream SSE stream: `n` chunk frames then `[DONE]`.
// Same fixture-building idiom (and lint trade-off) as tests/chat.rs.
#[allow(clippy::format_collect)]
fn sse_body(n: usize) -> String {
    let frames: String = (0..n)
        .map(|i| {
            format!(
                "data: {{\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"tok{i}\"}}}}]}}\n\n"
            )
        })
        .collect();
    format!("{frames}data: [DONE]\n\n")
}

/// Spawn the full app (no auth, default guards) on an ephemeral port and
/// return its base URL. Mirrors the integration tests' `common::spawn_with`,
/// which benches cannot import.
async fn spawn_gateway(registry: Arc<Registry>) -> String {
    let metrics = Metrics::new();
    let tokens = TokenMetrics::register(&metrics, &[]).expect("register token metrics");
    let latency = LatencyMetrics::register(&metrics).expect("register latency metrics");
    let state = AppState::new(metrics, registry, tokens, latency).with_body_limit(10 * 1024 * 1024);

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("read local addr");
    let app = build_app(state);
    tokio::spawn(async move {
        let _ = serve(
            listener,
            app,
            Duration::from_secs(5),
            std::future::pending(),
        )
        .await;
    });
    format!("http://{addr}")
}

/// Time-to-first-chunk of a streaming chat completion, direct vs via LUMEN.
fn bench_stream_ttfb(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");

    // One upstream and one gateway for the whole bench; `upstream` must stay
    // alive in this scope or the mock server shuts down mid-bench.
    let (upstream, gateway_base) = rt.block_on(async {
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse_body(8)),
            )
            .mount(&upstream)
            .await;
        let base = spawn_gateway(openai_registry(&upstream.uri())).await;
        (upstream, base)
    });

    let client = reqwest::Client::new();
    let request = json!({
        "model": "gpt",
        "messages": [{ "role": "user", "content": "ping" }],
        "stream": true
    });

    let mut group = c.benchmark_group("stream_ttfb");
    let targets = [
        (
            "direct_to_upstream",
            format!("{}/chat/completions", upstream.uri()),
        ),
        ("via_gateway", format!("{gateway_base}/v1/chat/completions")),
    ];
    for (id, url) in targets {
        group.bench_function(id, |b| {
            b.to_async(&rt).iter_custom(|iters| {
                let client = client.clone();
                let url = url.clone();
                let request = request.clone();
                async move {
                    let mut in_first_bit = Duration::ZERO;
                    for _ in 0..iters {
                        // Timed span: request dispatched -> first body bytes.
                        let start = Instant::now();
                        let mut resp = client
                            .post(&url)
                            .json(&request)
                            .send()
                            .await
                            .expect("request sent");
                        let first = resp.chunk().await.expect("read first chunk");
                        in_first_bit += start.elapsed();

                        assert_eq!(resp.status(), 200, "bench target answered non-200");
                        assert!(first.is_some(), "stream produced no body");
                        // Drain the rest outside the timed span so every
                        // iteration completes the stream normally (measuring
                        // the happy path, not the client-cancel path) and the
                        // connection returns to the client's pool.
                        while resp.chunk().await.expect("drain stream").is_some() {}
                    }
                    in_first_bit
                }
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_stream_ttfb);
criterion_main!(benches);
