//! ADR 004 (2026-11-12 amendment): the OpenAI-compatible providers hand a
//! non-streaming upstream body back as the bytes received when nothing in it
//! has to change, and as a typed response otherwise.

use lumen_core::{ChatCompletion, ChatMessage, ChatProvider, ChatRequest, MessageContent};
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
