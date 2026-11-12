//! Per-provider multimodal embedding translation (M9). Asserts the exact
//! upstream JSON a `Multi` (content-parts) request produces, and that a
//! text-only request still uses each provider's existing shape.

use std::sync::Arc;

use lumen_core::{ContentPart, EmbedInput, EmbedItem, EmbedRequest, EmbeddingProvider, ImageUrl};
use lumen_providers::{http, CohereProvider, JinaProvider, VoyageProvider};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const DATA_URI: &str = "data:image/png;base64,QUJD";

/// A `Multi` request: one item carrying a caption + an inline image.
fn multimodal_request(model: &str) -> EmbedRequest {
    EmbedRequest {
        model: model.to_owned(),
        input: EmbedInput::Multi(vec![EmbedItem::Parts(vec![
            ContentPart {
                kind: "text".to_owned(),
                text: Some("a caption".to_owned()),
                image_url: None,
                extra: serde_json::Map::new(),
            },
            ContentPart {
                kind: "image_url".to_owned(),
                text: None,
                image_url: Some(ImageUrl {
                    url: DATA_URI.to_owned(),
                    detail: None,
                }),
                extra: serde_json::Map::new(),
            },
        ])]),
        encoding_format: None,
        dimensions: None,
        user: None,
        extra: serde_json::Map::new(),
    }
}

async fn sent_body(mock: &MockServer) -> Value {
    let requests = mock.received_requests().await.unwrap();
    serde_json::from_slice(&requests[0].body).unwrap()
}

#[tokio::test]
async fn cohere_multimodal_uses_inputs_content_array() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "embeddings": { "float": [[0.1, 0.2]] },
            "meta": { "billed_units": { "input_tokens": 5 } }
        })))
        .mount(&upstream)
        .await;

    let provider: Arc<dyn EmbeddingProvider> = Arc::new(CohereProvider::new(
        http::build_client(),
        "cohere-test",
        Some(upstream.uri()),
        Some("sk-x".to_owned()),
    ));

    let resp = provider
        .embed(multimodal_request("embed-v4.0"), CancellationToken::new())
        .await
        .expect("embed ok");
    assert_eq!(resp.data.len(), 1);
    assert_eq!(resp.usage.prompt_tokens, 5);

    let body = sent_body(&upstream).await;
    // Multimodal → `inputs` content array, NOT top-level `texts`.
    assert!(
        body.get("texts").is_none(),
        "must not send `texts` for multimodal"
    );
    let content = &body["inputs"][0]["content"];
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[0]["text"], "a caption");
    assert_eq!(content[1]["type"], "image_url");
    assert_eq!(content[1]["image_url"]["url"], DATA_URI);
}

#[tokio::test]
async fn cohere_multimodal_honors_input_type_override() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "embeddings": { "float": [[0.1, 0.2]] },
            "meta": { "billed_units": { "input_tokens": 5 } }
        })))
        .mount(&upstream)
        .await;

    let provider: Arc<dyn EmbeddingProvider> = Arc::new(CohereProvider::new(
        http::build_client(),
        "cohere-test",
        Some(upstream.uri()),
        Some("sk-x".to_owned()),
    ));

    let mut req = multimodal_request("embed-v4.0");
    req.extra
        .insert("input_type".to_owned(), json!("search_query"));
    provider
        .embed(req, CancellationToken::new())
        .await
        .expect("embed ok");

    let body = sent_body(&upstream).await;
    assert_eq!(body["input_type"], "search_query");
}

#[tokio::test]
async fn cohere_text_only_still_uses_texts() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "embeddings": { "float": [[0.1]] },
            "meta": { "billed_units": { "input_tokens": 1 } }
        })))
        .mount(&upstream)
        .await;

    let provider: Arc<dyn EmbeddingProvider> = Arc::new(CohereProvider::new(
        http::build_client(),
        "cohere-test",
        Some(upstream.uri()),
        Some("sk-x".to_owned()),
    ));

    let req = EmbedRequest {
        model: "embed-english-v3.0".to_owned(),
        input: EmbedInput::Batch(vec!["hello".to_owned()]),
        encoding_format: None,
        dimensions: None,
        user: None,
        extra: serde_json::Map::new(),
    };
    provider.embed(req, CancellationToken::new()).await.unwrap();

    let body = sent_body(&upstream).await;
    assert_eq!(body["texts"][0], "hello");
    assert!(body.get("inputs").is_none());
}

#[tokio::test]
async fn voyage_multimodal_uses_multimodal_endpoint_and_content() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/multimodalembeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{ "embedding": [0.3, 0.4] }],
            "usage": { "total_tokens": 7 }
        })))
        .mount(&upstream)
        .await;

    let provider: Arc<dyn EmbeddingProvider> = Arc::new(VoyageProvider::new(
        http::build_client(),
        "voyage-test",
        Some(upstream.uri()),
        Some("sk-x".to_owned()),
    ));

    let resp = provider
        .embed(
            multimodal_request("voyage-multimodal-3"),
            CancellationToken::new(),
        )
        .await
        .expect("embed ok");
    assert_eq!(resp.data.len(), 1);
    assert_eq!(resp.data[0].embedding, vec![0.3, 0.4]);
    assert_eq!(resp.usage.prompt_tokens, 7);

    let body = sent_body(&upstream).await;
    let content = &body["inputs"][0]["content"];
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[0]["text"], "a caption");
    assert_eq!(content[1]["type"], "image_base64");
    assert_eq!(content[1]["image_base64"], DATA_URI);
}

#[tokio::test]
async fn voyage_text_only_uses_openai_embeddings_endpoint() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list",
            "data": [{ "object": "embedding", "index": 0, "embedding": [0.1] }],
            "model": "voyage-3",
            "usage": { "prompt_tokens": 2, "total_tokens": 2 }
        })))
        .mount(&upstream)
        .await;

    let provider: Arc<dyn EmbeddingProvider> = Arc::new(VoyageProvider::new(
        http::build_client(),
        "voyage-test",
        Some(upstream.uri()),
        Some("sk-x".to_owned()),
    ));

    let req = EmbedRequest {
        model: "voyage-3".to_owned(),
        input: EmbedInput::Batch(vec!["hello".to_owned()]),
        encoding_format: None,
        dimensions: None,
        user: None,
        extra: serde_json::Map::new(),
    };
    provider.embed(req, CancellationToken::new()).await.unwrap();

    let body = sent_body(&upstream).await;
    assert_eq!(body["input"][0], "hello");
}

/// Voyage's multimodal models answer 400 on `/embeddings`, even for plain
/// text: text-only input to one of them goes to `/multimodalembeddings`, one
/// text item per input, in order.
#[tokio::test]
async fn voyage_multimodal_model_text_only_uses_multimodal_endpoint() {
    let upstream = MockServer::start().await;
    let provider: Arc<dyn EmbeddingProvider> = Arc::new(VoyageProvider::new(
        http::build_client(),
        "voyage-test",
        Some(upstream.uri()),
        Some("sk-x".to_owned()),
    ));

    for (input, texts) in [
        (EmbedInput::Single("hello".to_owned()), vec!["hello"]),
        (
            EmbedInput::Batch(vec!["a".to_owned(), "b".to_owned()]),
            vec!["a", "b"],
        ),
    ] {
        upstream.reset().await;
        Mock::given(method("POST"))
            .and(path("/multimodalembeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": texts.iter().map(|_| json!({ "embedding": [0.1] })).collect::<Vec<_>>(),
                "usage": { "total_tokens": 4 }
            })))
            .mount(&upstream)
            .await;

        let req = EmbedRequest {
            model: "voyage-multimodal-3.5".to_owned(),
            input,
            encoding_format: None,
            dimensions: None,
            user: None,
            extra: serde_json::Map::new(),
        };
        let resp = provider
            .embed(req, CancellationToken::new())
            .await
            .expect("embed ok");
        assert_eq!(resp.data.len(), texts.len());

        let body = sent_body(&upstream).await;
        assert_eq!(body["model"], "voyage-multimodal-3.5");
        let inputs = body["inputs"].as_array().expect("inputs array");
        assert_eq!(inputs.len(), texts.len());
        for (input, text) in inputs.iter().zip(&texts) {
            assert_eq!(input["content"][0]["type"], "text");
            assert_eq!(input["content"][0]["text"], *text);
        }
    }
}

/// `dimensions` reaches `/multimodalembeddings` as Voyage's
/// `output_dimension` (never silently dropped), and is omitted when unset.
#[tokio::test]
async fn voyage_multimodal_forwards_dimensions_as_output_dimension() {
    let upstream = MockServer::start().await;
    let provider: Arc<dyn EmbeddingProvider> = Arc::new(VoyageProvider::new(
        http::build_client(),
        "voyage-test",
        Some(upstream.uri()),
        Some("sk-x".to_owned()),
    ));

    for dimensions in [Some(512), None] {
        upstream.reset().await;
        Mock::given(method("POST"))
            .and(path("/multimodalembeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [{ "embedding": [0.1] }],
                "usage": { "total_tokens": 1 }
            })))
            .mount(&upstream)
            .await;

        let req = EmbedRequest {
            model: "voyage-multimodal-3.5".to_owned(),
            input: EmbedInput::Single("hello".to_owned()),
            encoding_format: None,
            dimensions,
            user: None,
            extra: serde_json::Map::new(),
        };
        provider
            .embed(req, CancellationToken::new())
            .await
            .expect("embed ok");

        let body = sent_body(&upstream).await;
        match dimensions {
            Some(d) => assert_eq!(body["output_dimension"], d),
            None => assert!(body.get("output_dimension").is_none()),
        }
    }
}

#[tokio::test]
async fn jina_multimodal_uses_input_object_array() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list",
            "data": [
                { "object": "embedding", "index": 0, "embedding": [0.1] },
                { "object": "embedding", "index": 1, "embedding": [0.2] }
            ],
            "model": "jina-clip-v2",
            "usage": { "prompt_tokens": 4, "total_tokens": 4 }
        })))
        .mount(&upstream)
        .await;

    let provider: Arc<dyn EmbeddingProvider> = Arc::new(JinaProvider::new(
        http::build_client(),
        "jina-test",
        Some(upstream.uri()),
        Some("sk-x".to_owned()),
    ));

    // Two single-modality items: one text, one image.
    let req = EmbedRequest {
        model: "jina-clip-v2".to_owned(),
        input: EmbedInput::Multi(vec![
            EmbedItem::Text("a caption".to_owned()),
            EmbedItem::Parts(vec![ContentPart {
                kind: "image_url".to_owned(),
                text: None,
                image_url: Some(ImageUrl {
                    url: DATA_URI.to_owned(),
                    detail: None,
                }),
                extra: serde_json::Map::new(),
            }]),
        ]),
        encoding_format: None,
        dimensions: None,
        user: None,
        extra: serde_json::Map::new(),
    };
    provider.embed(req, CancellationToken::new()).await.unwrap();

    let body = sent_body(&upstream).await;
    assert_eq!(body["input"][0]["text"], "a caption");
    assert_eq!(body["input"][1]["image"], DATA_URI);
}

/// Issue #90 review: jina joins a multi-text-part item's fragments through the
/// shared `EmbedItem::joined_text` (newline-separated), NOT its own empty
/// concat that would fuse tokens across the boundary.
#[tokio::test]
async fn jina_multi_text_part_item_joins_fragments_with_newline() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list",
            "data": [{ "object": "embedding", "index": 0, "embedding": [0.1] }],
            "model": "jina-clip-v2",
            "usage": { "prompt_tokens": 4, "total_tokens": 4 }
        })))
        .mount(&upstream)
        .await;

    let provider: Arc<dyn EmbeddingProvider> = Arc::new(JinaProvider::new(
        http::build_client(),
        "jina-test",
        Some(upstream.uri()),
        Some("sk-x".to_owned()),
    ));

    let req = EmbedRequest {
        model: "jina-clip-v2".to_owned(),
        input: EmbedInput::Multi(vec![EmbedItem::Parts(vec![
            ContentPart {
                kind: "text".to_owned(),
                text: Some("The quick brown".to_owned()),
                image_url: None,
                extra: serde_json::Map::new(),
            },
            ContentPart {
                kind: "text".to_owned(),
                text: Some("fox jumps".to_owned()),
                image_url: None,
                extra: serde_json::Map::new(),
            },
        ])]),
        encoding_format: None,
        dimensions: None,
        user: None,
        extra: serde_json::Map::new(),
    };
    provider.embed(req, CancellationToken::new()).await.unwrap();

    let body = sent_body(&upstream).await;
    assert_eq!(body["input"][0]["text"], "The quick brown\nfox jumps");
}

/// Voyage's `/embeddings` is not the OpenAI schema: the width is
/// `output_dimension`, there is no `user`, and `encoding_format` only accepts
/// `base64`. The text path sends an explicit body with Voyage's own fields,
/// never the raw OpenAI request.
#[tokio::test]
async fn voyage_text_path_sends_only_fields_voyage_accepts() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list",
            "data": [{ "object": "embedding", "index": 0, "embedding": [0.1] }],
            "model": "voyage-3.5",
            "usage": { "total_tokens": 2 }
        })))
        .mount(&upstream)
        .await;
    let provider: Arc<dyn EmbeddingProvider> = Arc::new(VoyageProvider::new(
        http::build_client(),
        "voyage-test",
        Some(upstream.uri()),
        Some("sk-x".to_owned()),
    ));

    let req = EmbedRequest {
        model: "voyage-3.5".to_owned(),
        input: EmbedInput::Batch(vec!["hello".to_owned()]),
        encoding_format: Some("float".to_owned()),
        dimensions: Some(512),
        user: Some("end-user-1".to_owned()),
        extra: serde_json::Map::new(),
    };
    provider.embed(req, CancellationToken::new()).await.unwrap();
    assert_eq!(
        sent_body(&upstream).await,
        json!({ "model": "voyage-3.5", "input": ["hello"], "output_dimension": 512 })
    );

    // A single string stays a string; no width is sent when none was asked.
    upstream.reset().await;
    Mock::given(method("POST"))
        .and(path("/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list",
            "data": [{ "object": "embedding", "index": 0, "embedding": [0.1] }],
            "model": "voyage-3.5",
            "usage": { "total_tokens": 2 }
        })))
        .mount(&upstream)
        .await;
    let req = EmbedRequest {
        model: "voyage-3.5".to_owned(),
        input: EmbedInput::Single("hello".to_owned()),
        encoding_format: None,
        dimensions: None,
        user: None,
        extra: serde_json::Map::new(),
    };
    provider.embed(req, CancellationToken::new()).await.unwrap();
    assert_eq!(
        sent_body(&upstream).await,
        json!({ "model": "voyage-3.5", "input": "hello" })
    );
}

/// A requested `dimensions` reaches Cohere's multimodal body as
/// `output_dimension` (embed-v4), and is omitted when unset.
#[tokio::test]
async fn cohere_multimodal_maps_dimensions_to_output_dimension() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "embeddings": { "float": [[0.1, 0.2]] },
            "meta": { "billed_units": { "input_tokens": 5 } }
        })))
        .mount(&upstream)
        .await;
    let provider: Arc<dyn EmbeddingProvider> = Arc::new(CohereProvider::new(
        http::build_client(),
        "cohere-test",
        Some(upstream.uri()),
        Some("sk-x".to_owned()),
    ));

    let mut req = multimodal_request("embed-v4.0");
    req.dimensions = Some(512);
    provider.embed(req, CancellationToken::new()).await.unwrap();
    let body = sent_body(&upstream).await;
    assert_eq!(body["output_dimension"], 512);

    upstream.reset().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "embeddings": { "float": [[0.1, 0.2]] }
        })))
        .mount(&upstream)
        .await;
    provider
        .embed(multimodal_request("embed-v4.0"), CancellationToken::new())
        .await
        .unwrap();
    let body = sent_body(&upstream).await;
    assert!(body.get("output_dimension").is_none(), "got body: {body}");
}

/// Jina's multimodal body carries `dimensions` (its own parameter name) when
/// the client sets it, and omits it otherwise.
#[tokio::test]
async fn jina_multimodal_forwards_dimensions() {
    let upstream = MockServer::start().await;
    let provider: Arc<dyn EmbeddingProvider> = Arc::new(JinaProvider::new(
        http::build_client(),
        "jina-test",
        Some(upstream.uri()),
        Some("sk-x".to_owned()),
    ));

    for dimensions in [Some(512), None] {
        upstream.reset().await;
        Mock::given(method("POST"))
            .and(path("/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "object": "list",
                "data": [{ "object": "embedding", "index": 0, "embedding": [0.1] }],
                "model": "jina-clip-v2",
                "usage": { "prompt_tokens": 4, "total_tokens": 4 }
            })))
            .mount(&upstream)
            .await;

        let mut req = multimodal_request("jina-clip-v2");
        req.dimensions = dimensions;
        provider.embed(req, CancellationToken::new()).await.unwrap();

        let body = sent_body(&upstream).await;
        match dimensions {
            Some(d) => assert_eq!(body["dimensions"], d),
            None => assert!(body.get("dimensions").is_none(), "got body: {body}"),
        }
    }
}
