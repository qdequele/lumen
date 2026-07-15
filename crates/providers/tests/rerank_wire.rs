//! Wire-shape and auth-header conformance for the M-added rerankers.
//!
//! The shared suite in `rerank.rs` proves response translation (index mapping,
//! ordering, error handling, cancellation) is correct. This file proves the
//! *request* side: each provider must hit its provider's REAL endpoint path
//! (the responder is pinned to it with a wiremock `path` matcher, so a wrong
//! path 404s and the test fails), send its own documented schema (renamed
//! fields, nested objects), and use its own auth header shape without leaking
//! the key anywhere else. Each provider is built with a base URL carrying the
//! same path suffix as its real default base (`/v1` for Mixedbread and
//! Together, the bare root for Pinecone and NVIDIA), so the asserted full path
//! is exactly what production traffic would use.

#![allow(clippy::float_cmp)]

use lumen_core::{RerankDocument, RerankProvider, RerankRequest};
use lumen_providers::{
    MixedbreadProvider, NvidiaProvider, PineconeProvider, TogetherRerankProvider,
};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const KEY: &str = "sk-test-xxx";

fn request_top2() -> RerankRequest {
    RerankRequest {
        model: "test-model".to_owned(),
        query: "what is rust".to_owned(),
        documents: ["alpha", "beta", "gamma"]
            .iter()
            .map(|s| RerankDocument::Text((*s).to_owned()))
            .collect(),
        top_n: Some(2),
        return_documents: false,
        rank_fields: None,
    }
}

/// Mount a POST responder pinned to the provider's real `endpoint_path`,
/// returning `body`. Any request to a different path gets wiremock's 404, so a
/// provider posting to the wrong endpoint fails the test.
async fn mock_returning(endpoint_path: &str, body: Value) -> MockServer {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(endpoint_path))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&mock)
        .await;
    mock
}

fn recorded_body(req: &Request) -> Value {
    serde_json::from_slice(&req.body).expect("request body is JSON")
}

fn header(req: &Request, name: &str) -> Option<String> {
    req.headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

// --------------------------------------------------------------------------
// Mixedbread: POST /v1/reranking; bearer auth; request uses `input` + `top_k`.
// --------------------------------------------------------------------------

#[tokio::test]
async fn mixedbread_posts_v1_reranking_with_input_top_k_and_bearer_auth() {
    // Pinned to Mixedbread's real endpoint path: the default base is
    // `https://api.mixedbread.com/v1` and the endpoint is POST /v1/reranking
    // (NOT /v1/rerank), so the base here carries the same /v1 suffix.
    let mock = mock_returning(
        "/v1/reranking",
        json!({ "data": [{ "index": 1, "score": 0.9 }] }),
    )
    .await;
    let provider = MixedbreadProvider::new(
        reqwest::Client::new(),
        "mxbai",
        Some(format!("{}/v1", mock.uri())),
        Some(KEY.to_owned()),
    );

    let resp = provider
        .rerank(request_top2(), CancellationToken::new())
        .await
        .expect("rerank ok");
    assert_eq!(resp.results[0].index, 1);

    let reqs = mock.received_requests().await.unwrap();
    assert_eq!(reqs[0].url.path(), "/v1/reranking");
    let body = recorded_body(&reqs[0]);
    assert_eq!(body["input"], json!(["alpha", "beta", "gamma"]));
    assert_eq!(body["top_k"], json!(2));
    assert!(body.get("documents").is_none(), "must not send `documents`");
    assert!(body.get("top_n").is_none(), "must not send `top_n`");
    assert_eq!(
        header(&reqs[0], "authorization").as_deref(),
        Some("Bearer sk-test-xxx")
    );
    // Token-billed: no upstream search units, so the gateway will estimate.
    assert_eq!(resp.usage.search_units, 0);
}

// --------------------------------------------------------------------------
// Pinecone: `Api-Key` header (not bearer); documents are `{text}` objects;
// upstream `rerank_units` is carried through as `search_units`.
// --------------------------------------------------------------------------

#[tokio::test]
async fn pinecone_sends_object_documents_with_api_key_header_and_carries_units() {
    // Pinecone's real endpoint is POST /rerank on the API root
    // (`https://api.pinecone.io`), so the base is the bare mock root.
    let mock = mock_returning(
        "/rerank",
        json!({
            "data": [{ "index": 2, "score": 0.7 }],
            "usage": { "rerank_units": 3 }
        }),
    )
    .await;
    let provider = PineconeProvider::new(
        reqwest::Client::new(),
        "pinecone",
        Some(mock.uri()),
        Some("pcsk-secret".to_owned()),
    );

    let resp = provider
        .rerank(request_top2(), CancellationToken::new())
        .await
        .expect("rerank ok");
    assert_eq!(resp.results[0].index, 2);
    // Pinecone reports real usage: carried through verbatim, not estimated.
    assert_eq!(resp.usage.search_units, 3);
    assert_eq!(resp.usage.estimated, None);

    let reqs = mock.received_requests().await.unwrap();
    assert_eq!(reqs[0].url.path(), "/rerank");
    let body = recorded_body(&reqs[0]);
    assert_eq!(
        body["documents"],
        json!([{ "text": "alpha" }, { "text": "beta" }, { "text": "gamma" }])
    );
    assert_eq!(body["top_n"], json!(2));
    // Auth is the `Api-Key` header plus a pinned version, never a bearer token.
    assert_eq!(header(&reqs[0], "api-key").as_deref(), Some("pcsk-secret"));
    assert!(
        header(&reqs[0], "x-pinecone-api-version").is_some(),
        "must send the pinned API-version header"
    );
    assert!(
        header(&reqs[0], "authorization").is_none(),
        "Pinecone must not use bearer auth"
    );
}

// --------------------------------------------------------------------------
// NVIDIA NIM: `/v1/ranking`; query/passages objects; logit -> relevance_score.
// --------------------------------------------------------------------------

#[tokio::test]
async fn nvidia_sends_query_and_passages_to_ranking_path_passing_logits_through() {
    // NIM's real endpoint is POST {root}/v1/ranking; the configured base is the
    // NIM root, so the mock root plus the provider-appended path is the real one.
    let mock = mock_returning(
        "/v1/ranking",
        json!({
            "rankings": [{ "index": 0, "logit": -1.5 }, { "index": 1, "logit": 4.2 }]
        }),
    )
    .await;
    let provider = NvidiaProvider::new(
        reqwest::Client::new(),
        "nvidia",
        mock.uri(),
        Some("nvapi-secret".to_owned()),
    );

    let resp = provider
        .rerank(request_top2(), CancellationToken::new())
        .await
        .expect("rerank ok");
    // Logits pass through unchanged (including the negative one).
    let by_index: Vec<(u32, f32)> = resp
        .results
        .iter()
        .map(|r| (r.index, r.relevance_score))
        .collect();
    assert!(by_index.contains(&(0, -1.5)));
    assert!(by_index.contains(&(1, 4.2)));

    let reqs = mock.received_requests().await.unwrap();
    assert_eq!(reqs[0].url.path(), "/v1/ranking");
    let body = recorded_body(&reqs[0]);
    assert_eq!(body["query"], json!({ "text": "what is rust" }));
    assert_eq!(
        body["passages"],
        json!([{ "text": "alpha" }, { "text": "beta" }, { "text": "gamma" }])
    );
    assert_eq!(
        header(&reqs[0], "authorization").as_deref(),
        Some("Bearer nvapi-secret")
    );
}

#[tokio::test]
async fn nvidia_works_without_a_key() {
    let mock = mock_returning(
        "/v1/ranking",
        json!({ "rankings": [{ "index": 0, "logit": 1.0 }] }),
    )
    .await;
    let provider = NvidiaProvider::new(reqwest::Client::new(), "nvidia", mock.uri(), None);

    provider
        .rerank(request_top2(), CancellationToken::new())
        .await
        .expect("keyless rerank ok");

    let reqs = mock.received_requests().await.unwrap();
    assert!(
        header(&reqs[0], "authorization").is_none(),
        "no key configured => no auth header"
    );
}

// --------------------------------------------------------------------------
// Together: POST /v1/rerank; bearer auth; Cohere-shaped `documents` + `top_n`.
// --------------------------------------------------------------------------

#[tokio::test]
async fn together_posts_v1_rerank_with_documents_top_n_and_bearer_auth() {
    // Together's real endpoint is POST /v1/rerank (default base
    // `https://api.together.xyz/v1` + `/rerank`), so the base here carries the
    // same /v1 suffix.
    let mock = mock_returning(
        "/v1/rerank",
        json!({ "results": [{ "index": 0, "relevance_score": 0.5 }] }),
    )
    .await;
    let provider = TogetherRerankProvider::new(
        reqwest::Client::new(),
        "together",
        Some(format!("{}/v1", mock.uri())),
        Some(KEY.to_owned()),
    );

    provider
        .rerank(request_top2(), CancellationToken::new())
        .await
        .expect("rerank ok");

    let reqs = mock.received_requests().await.unwrap();
    assert_eq!(reqs[0].url.path(), "/v1/rerank");
    let body = recorded_body(&reqs[0]);
    assert_eq!(body["documents"], json!(["alpha", "beta", "gamma"]));
    assert_eq!(body["top_n"], json!(2));
    assert_eq!(
        header(&reqs[0], "authorization").as_deref(),
        Some("Bearer sk-test-xxx")
    );
}
