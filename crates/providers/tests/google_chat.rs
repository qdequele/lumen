//! Google Gemini Developer API chat (`POST /v1beta/models/{model}:generateContent`)
//! wire conformance for the OpenAI chat extras mapped onto `generationConfig`
//! (issue #91): `frequency_penalty` / `presence_penalty` must reach the actual
//! upstream body instead of being silently dropped.

use futures::StreamExt;
use lumen_core::{ChatMessage, ChatProvider, ChatRequest, MessageContent, ProviderError};
use lumen_providers::GoogleProvider;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const DUMMY_KEY: &str = "goog-test-xxx";
const MODEL: &str = "gemini-2.0-flash";

fn provider(base_url: String) -> GoogleProvider {
    GoogleProvider::new(
        reqwest::Client::new(),
        "google-test",
        Some(base_url),
        Some(DUMMY_KEY.to_owned()),
    )
}

fn request(content: &str) -> ChatRequest {
    ChatRequest {
        model: MODEL.to_owned(),
        messages: vec![ChatMessage {
            role: "user".to_owned(),
            content: Some(MessageContent::Text(content.to_owned())),
            name: None,
            extra: serde_json::Map::new(),
        }],
        temperature: None,
        top_p: None,
        max_tokens: Some(64),
        n: None,
        stop: None,
        stream: false,
        extra: serde_json::Map::new(),
    }
}

fn gemini_response(text: &str) -> Value {
    json!({
        "candidates": [{
            "content": { "parts": [{ "text": text }], "role": "model" },
            "finishReason": "STOP"
        }],
        "usageMetadata": {
            "promptTokenCount": 5, "candidatesTokenCount": 3, "totalTokenCount": 8
        }
    })
}

/// Issue #91: `frequency_penalty` / `presence_penalty` map to Gemini's native
/// `generationConfig.frequencyPenalty` / `presencePenalty` instead of being
/// silently dropped - proven on the actual wire body.
#[tokio::test]
async fn frequency_and_presence_penalty_reach_the_gemini_wire() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/v1beta/models/{MODEL}:generateContent")))
        .respond_with(ResponseTemplate::new(200).set_body_json(gemini_response("hi")))
        .mount(&mock)
        .await;

    let p = provider(mock.uri());
    let mut req = request("hello");
    req.extra.insert("frequency_penalty".to_owned(), json!(0.5));
    req.extra
        .insert("presence_penalty".to_owned(), json!(-0.25));
    p.chat(req, CancellationToken::new()).await.unwrap();

    let sent: Value =
        serde_json::from_slice(&mock.received_requests().await.unwrap()[0].body).unwrap();
    assert_eq!(sent["generationConfig"]["frequencyPenalty"], 0.5);
    assert_eq!(sent["generationConfig"]["presencePenalty"], -0.25);
}

/// A prompt-level safety block (HTTP 200, `promptFeedback.blockReason`, no
/// candidates) surfaces as a content-filter refusal naming the provider, in
/// both the non-streaming and streaming paths.
#[tokio::test]
async fn prompt_level_block_is_content_filtered_on_both_paths() {
    let mock = MockServer::start().await;
    let blocked = json!({
        "promptFeedback": { "blockReason": "SAFETY" },
        "usageMetadata": { "promptTokenCount": 5, "totalTokenCount": 5 }
    });
    Mock::given(method("POST"))
        .and(path(format!("/v1beta/models/{MODEL}:generateContent")))
        .respond_with(ResponseTemplate::new(200).set_body_json(blocked.clone()))
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/v1beta/models/{MODEL}:streamGenerateContent")))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(format!("data: {blocked}\n\n")),
        )
        .mount(&mock)
        .await;

    let p = provider(mock.uri());
    let err = p
        .chat(request("blocked"), CancellationToken::new())
        .await
        .unwrap_err();
    assert!(
        matches!(&err, ProviderError::ContentFiltered { provider, .. } if provider == "google-test"),
        "{err:?}"
    );

    let mut stream = p
        .chat_stream(request("blocked"), CancellationToken::new())
        .await
        .unwrap();
    let first = stream.next().await.expect("one item");
    assert!(
        matches!(&first, Err(ProviderError::ContentFiltered { provider, .. }) if provider == "google-test"),
        "{first:?}"
    );
}
