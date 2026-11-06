//! A client that sends only `max_completion_tokens` (the field OpenAI
//! reasoning models require) must still get its output cap honoured by the
//! Anthropic provider, which only knows `max_tokens`.

use lumen_core::{ChatMessage, ChatProvider, ChatRequest, MessageContent};
use lumen_providers::AnthropicProvider;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn request(max_tokens: Option<u32>) -> ChatRequest {
    let mut extra = serde_json::Map::new();
    extra.insert("max_completion_tokens".to_owned(), json!(500));
    ChatRequest {
        model: "claude-x".to_owned(),
        messages: vec![ChatMessage {
            role: "user".to_owned(),
            content: Some(MessageContent::Text("hi".to_owned())),
            name: None,
            extra: serde_json::Map::new(),
        }],
        temperature: None,
        top_p: None,
        max_tokens,
        n: None,
        stop: None,
        stream: false,
        extra,
    }
}

async fn provider_and_upstream(expected_max_tokens: u32) -> (AnthropicProvider, MockServer) {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(body_partial_json(
            json!({ "max_tokens": expected_max_tokens }),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "claude-x",
            "content": [{ "type": "text", "text": "ok" }],
            "stop_reason": "end_turn",
            "usage": { "input_tokens": 3, "output_tokens": 1 }
        })))
        .expect(1)
        .mount(&upstream)
        .await;
    let provider = AnthropicProvider::new(
        reqwest::Client::new(),
        "anthropic-test",
        Some(upstream.uri()),
        Some("sk-ant-test".to_owned()),
    );
    (provider, upstream)
}

#[tokio::test]
async fn chat_forwards_max_completion_tokens_as_max_tokens() {
    let (provider, upstream) = provider_and_upstream(500).await;
    provider
        .chat(request(None), CancellationToken::new())
        .await
        .unwrap();
    // `.expect(1)` is verified on drop; also assert the body explicitly so a
    // mismatch reads clearly.
    let received = upstream.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    let sent: Value = serde_json::from_slice(&received[0].body).unwrap();
    assert_eq!(sent["max_tokens"], 500);
}

#[tokio::test]
async fn chat_prefers_max_tokens_over_max_completion_tokens() {
    let (provider, _upstream) = provider_and_upstream(100).await;
    provider
        .chat(request(Some(100)), CancellationToken::new())
        .await
        .unwrap();
}
