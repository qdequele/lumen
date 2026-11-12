//! An Anthropic `error` event arriving inside a 200 stream keeps the class
//! the same error has as an HTTP status (an in-stream `overloaded_error` is a
//! retryable upstream 529 naming the provider), instead of reading as an
//! unparseable upstream response (LM-3002). Events after it are not
//! translated, and the upstream message never reaches the error.

use futures::StreamExt;
use lumen_core::{ChatMessage, ChatProvider, ChatRequest, MessageContent, ProviderError};
use lumen_providers::AnthropicProvider;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn request() -> ChatRequest {
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
        max_tokens: Some(16),
        n: None,
        stop: None,
        stream: true,
        extra: serde_json::Map::new(),
    }
}

#[tokio::test]
async fn in_stream_overloaded_error_is_a_retryable_upstream_error() {
    let upstream = MockServer::start().await;
    let body = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"claude-x\",\"usage\":{\"input_tokens\":3}}}\n\n",
        "event: error\n",
        "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"SECRET DETAIL\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"late\"}}\n\n",
    );
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&upstream)
        .await;

    let provider = AnthropicProvider::new(
        reqwest::Client::new(),
        "anthropic-test",
        Some(upstream.uri()),
        Some("sk-ant-test".to_owned()),
    );
    let items: Vec<_> = provider
        .chat_stream(request(), CancellationToken::new())
        .await
        .unwrap()
        .collect()
        .await;

    // The role chunk, then the classified error; the late delta is dropped.
    assert_eq!(items.len(), 2, "{items:?}");
    assert!(items[0].is_ok());
    let err = items[1].as_ref().unwrap_err();
    assert!(
        matches!(
            err,
            ProviderError::Upstream { provider, status: 529, retryable: true, .. }
                if provider == "anthropic-test"
        ),
        "{err:?}"
    );
    assert!(!format!("{err} {err:?}").contains("SECRET DETAIL"));
}
