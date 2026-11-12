//! ADR 004 (2026-11-12 amendment): the OpenAI-compatible providers hand a
//! non-streaming upstream body back as the bytes received when nothing in it
//! has to change, and as a typed response otherwise.

use lumen_core::{
    ChatCompletion, ChatMessage, ChatProvider, ChatRequest, EmbedCompletion, EmbedInput,
    EmbedRequest, EmbeddingProvider, MessageContent,
};
use lumen_providers::{AzureProvider, MistralProvider, OpenAiProvider};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Spacing, key order and unknown fields a typed round trip would not keep.
const CHAT_BODY: &str = "{ \"id\":\"c-1\",\"object\":\"chat.completion\",\"created\":7,\n \
    \"model\":\"m\",\"service_tier\":\"default\",\
    \"choices\":[{\"index\":0,\"message\":{\"role\":\"assistant\",\"content\":\"ok\"},\
    \"logprobs\":null,\"finish_reason\":\"stop\"}],\
    \"usage\":{\"total_tokens\":10,\"prompt_tokens\":8,\"completion_tokens\":2,\"cost\":0.5} }";

const CHAT_BODY_WITHOUT_USAGE: &str = r#"{"id":"c-1","object":"chat.completion","created":7,
    "model":"m","choices":[{"index":0,"message":{"role":"assistant","content":"ok"}}]}"#;

fn chat_request() -> ChatRequest {
    ChatRequest {
        model: "m".to_owned(),
        messages: vec![ChatMessage {
            role: "user".to_owned(),
            content: Some(MessageContent::Text("hi".to_owned())),
            name: None,
            extra: serde_json::Map::new(),
        }],
        temperature: None,
        top_p: None,
        max_tokens: None,
        n: None,
        stop: None,
        stream: false,
        extra: serde_json::Map::new(),
    }
}

async fn upstream_answering(body: &'static str) -> MockServer {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path_regex("chat/completions$"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "application/json"))
        .mount(&upstream)
        .await;
    upstream
}

/// The three OpenAI-compatible chat providers, pointed at `uri`.
fn chat_providers(uri: &str) -> Vec<(&'static str, Box<dyn ChatProvider>)> {
    let client = reqwest::Client::new();
    let key = Some("sk-test".to_owned());
    vec![
        (
            "openai",
            Box::new(OpenAiProvider::new(
                client.clone(),
                "openai",
                Some(uri.to_owned()),
                key.clone(),
            )),
        ),
        (
            "mistral",
            Box::new(MistralProvider::new(
                client.clone(),
                "mistral",
                Some(uri.to_owned()),
                key.clone(),
            )),
        ),
        (
            "azure",
            Box::new(AzureProvider::new(client, "azure", uri, None, key)),
        ),
    ]
}

#[tokio::test]
async fn chat_body_with_usage_is_returned_as_the_bytes_received() {
    let upstream = upstream_answering(CHAT_BODY).await;
    for (name, provider) in chat_providers(&upstream.uri()) {
        let completion = provider
            .chat_completion(chat_request(), CancellationToken::new())
            .await
            .unwrap();
        let ChatCompletion::Passthrough(body) = completion else {
            panic!("{name}: expected a passthrough");
        };
        assert_eq!(body.usage().prompt_tokens, 8, "{name}");
        assert_eq!(body.usage().completion_tokens, 2, "{name}");
        assert_eq!(body.into_body(), CHAT_BODY.as_bytes(), "{name}");
    }
}

#[tokio::test]
async fn chat_body_without_usage_is_returned_typed() {
    let upstream = upstream_answering(CHAT_BODY_WITHOUT_USAGE).await;
    for (name, provider) in chat_providers(&upstream.uri()) {
        let completion = provider
            .chat_completion(chat_request(), CancellationToken::new())
            .await
            .unwrap();
        let ChatCompletion::Typed(response) = completion else {
            panic!("{name}: expected the typed response");
        };
        assert_eq!(response.usage, None, "{name}");
        assert_eq!(response.choices.len(), 1, "{name}");
    }
}

#[tokio::test]
async fn chat_body_that_is_not_a_completion_is_a_translation_error() {
    let upstream =
        upstream_answering(r#"{"usage":{"prompt_tokens":8,"completion_tokens":2}}"#).await;
    for (name, provider) in chat_providers(&upstream.uri()) {
        let err = provider
            .chat_completion(chat_request(), CancellationToken::new())
            .await
            .unwrap_err();
        assert!(
            matches!(err, lumen_core::ProviderError::Translation(_)),
            "{name}: {err:?}"
        );
    }
}

/// Float vectors with more digits than an `f32` keeps, odd spacing and an
/// unknown usage field.
const EMBED_BODY: &str = "{ \"object\":\"list\",\"data\":[ {\"object\":\"embedding\",\
    \"index\":0,\"embedding\":[0.006929283495992422, -1e-3]} ],\n\
    \"model\":\"m\",\"usage\":{\"total_tokens\":3,\"prompt_tokens\":3,\"cost\":0.5} }";

fn embed_request(encoding_format: Option<&str>) -> EmbedRequest {
    EmbedRequest {
        model: "m".to_owned(),
        input: EmbedInput::Single("hi".to_owned()),
        encoding_format: encoding_format.map(str::to_owned),
        dimensions: None,
        user: None,
        extra: serde_json::Map::new(),
    }
}

/// The three OpenAI-compatible embedding providers, pointed at `uri`.
fn embed_providers(uri: &str) -> Vec<(&'static str, Box<dyn EmbeddingProvider>)> {
    let client = reqwest::Client::new();
    let key = Some("sk-test".to_owned());
    vec![
        (
            "openai",
            Box::new(OpenAiProvider::new(
                client.clone(),
                "openai",
                Some(uri.to_owned()),
                key.clone(),
            )),
        ),
        (
            "mistral",
            Box::new(MistralProvider::new(
                client.clone(),
                "mistral",
                Some(uri.to_owned()),
                key.clone(),
            )),
        ),
        (
            "azure",
            Box::new(AzureProvider::new(client, "azure", uri, None, key)),
        ),
    ]
}

async fn embed_upstream_answering(body: &'static str) -> MockServer {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path_regex("embeddings$"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "application/json"))
        .mount(&upstream)
        .await;
    upstream
}

#[tokio::test]
async fn embed_body_in_the_requested_encoding_is_returned_as_the_bytes_received() {
    let upstream = embed_upstream_answering(EMBED_BODY).await;
    for (name, provider) in embed_providers(&upstream.uri()) {
        for encoding_format in [None, Some("float")] {
            let completion = provider
                .embed_completion(embed_request(encoding_format), CancellationToken::new())
                .await
                .unwrap();
            assert_eq!(completion.vector_count(), 1, "{name}");
            let EmbedCompletion::Passthrough(body) = completion else {
                panic!("{name}: expected a passthrough");
            };
            assert_eq!(body.usage().prompt_tokens, 3, "{name}");
            assert_eq!(body.into_body(), EMBED_BODY.as_bytes(), "{name}");
        }
    }
}

#[tokio::test]
async fn embed_body_in_another_encoding_is_returned_typed() {
    // The client asked for base64 and the upstream answered with floats: the
    // gateway has to re-encode, so the vectors are decoded.
    let upstream = embed_upstream_answering(EMBED_BODY).await;
    for (name, provider) in embed_providers(&upstream.uri()) {
        let completion = provider
            .embed_completion(embed_request(Some("base64")), CancellationToken::new())
            .await
            .unwrap();
        let EmbedCompletion::Typed(response) = completion else {
            panic!("{name}: expected the typed response");
        };
        assert_eq!(response.data[0].embedding.len(), 2, "{name}");
        assert_eq!(response.usage.prompt_tokens, 3, "{name}");
    }
}
