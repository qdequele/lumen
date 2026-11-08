//! OpenAI `response_format: {"type": "json_schema"}` must reach Anthropic as
//! structured outputs (`output_config.format`), on both the non-streaming and
//! the streaming path, and even on a `strict` provider. Before the fix the
//! field was dropped, so a client asking for a JSON schema got free text
//! (e.g. `Apple` for a `{"fruit": string}` schema).

use lumen_core::{ChatMessage, ChatProvider, ChatRequest, MessageContent, ProviderError};
use lumen_providers::AnthropicProvider;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn request(response_format: Value) -> ChatRequest {
    let mut extra = serde_json::Map::new();
    extra.insert("response_format".to_owned(), response_format);
    ChatRequest {
        model: "claude-x".to_owned(),
        messages: vec![ChatMessage {
            role: "user".to_owned(),
            content: Some(MessageContent::Text("Name a fruit".to_owned())),
            name: None,
            extra: serde_json::Map::new(),
        }],
        temperature: None,
        top_p: None,
        max_tokens: Some(64),
        n: None,
        stop: None,
        stream: false,
        extra,
    }
}

fn fruit_schema() -> Value {
    json!({
        "type": "json_schema",
        "json_schema": {
            "name": "fruit",
            "strict": true,
            "schema": {
                "type": "object",
                "properties": { "fruit": { "type": "string" } },
                "required": ["fruit"],
                "additionalProperties": false
            }
        }
    })
}

async fn upstream() -> MockServer {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "claude-x",
            "content": [{ "type": "text", "text": "{\"fruit\":\"apple\"}" }],
            "stop_reason": "end_turn",
            "usage": { "input_tokens": 3, "output_tokens": 5 }
        })))
        .mount(&upstream)
        .await;
    upstream
}

fn provider(upstream: &MockServer, strict: bool) -> AnthropicProvider {
    AnthropicProvider::new(
        reqwest::Client::new(),
        "anthropic-test",
        Some(upstream.uri()),
        Some("sk-ant-test".to_owned()),
    )
    .with_strict(strict)
}

async fn sent_body(upstream: &MockServer) -> Value {
    let received = upstream.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    serde_json::from_slice(&received[0].body).unwrap()
}

#[tokio::test]
async fn json_schema_maps_to_output_config_format() {
    let upstream = upstream().await;
    let resp = provider(&upstream, false)
        .chat(request(fruit_schema()), CancellationToken::new())
        .await
        .unwrap();
    let sent = sent_body(&upstream).await;
    assert_eq!(
        sent["output_config"],
        json!({
            "format": {
                "type": "json_schema",
                "schema": {
                    "type": "object",
                    "properties": { "fruit": { "type": "string" } },
                    "required": ["fruit"],
                    "additionalProperties": false
                }
            }
        })
    );
    assert!(sent.get("response_format").is_none());
    let text = resp.choices[0]
        .message
        .content
        .as_ref()
        .unwrap()
        .text()
        .into_owned();
    assert_eq!(text, "{\"fruit\":\"apple\"}");
}

#[tokio::test]
async fn strict_provider_accepts_json_schema() {
    let upstream = upstream().await;
    provider(&upstream, true)
        .chat(request(fruit_schema()), CancellationToken::new())
        .await
        .unwrap();
    assert!(sent_body(&upstream).await["output_config"]["format"].is_object());
}

#[tokio::test]
async fn streaming_path_carries_output_config_too() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"),
        )
        .mount(&upstream)
        .await;
    let stream = provider(&upstream, false)
        .chat_stream(request(fruit_schema()), CancellationToken::new())
        .await
        .unwrap();
    drop(stream);
    let sent = sent_body(&upstream).await;
    assert_eq!(sent["stream"], true);
    assert_eq!(sent["output_config"]["format"]["type"], "json_schema");
}

#[tokio::test]
async fn text_format_is_the_default_and_never_rejected() {
    let upstream = upstream().await;
    provider(&upstream, true)
        .chat(request(json!({ "type": "text" })), CancellationToken::new())
        .await
        .unwrap();
    assert!(sent_body(&upstream).await.get("output_config").is_none());
}

/// Anthropic has no schema-less JSON mode: `json_object` stays unsupported
/// (rejected in strict mode before any upstream call).
#[tokio::test]
async fn json_object_is_still_unsupported_in_strict_mode() {
    let upstream = upstream().await;
    let err = provider(&upstream, true)
        .chat(
            request(json!({ "type": "json_object" })),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(&err, ProviderError::UnsupportedField { field, .. } if field == "response_format"),
        "{err:?}"
    );
    assert!(upstream.received_requests().await.unwrap().is_empty());
}

/// A `json_schema` with no schema cannot be expressed either.
#[tokio::test]
async fn json_schema_without_schema_is_unsupported_in_strict_mode() {
    let upstream = upstream().await;
    let err = provider(&upstream, true)
        .chat(
            request(json!({ "type": "json_schema", "json_schema": { "name": "x" } })),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ProviderError::UnsupportedField { .. }));
}

/// Lenient (default) mode: an inexpressible `response_format` (here
/// `json_object`, or a map schema) is dropped and the call still goes out,
/// with no `output_config`.
#[tokio::test]
async fn lenient_provider_drops_inexpressible_formats() {
    for format in [
        json!({ "type": "json_object" }),
        json!({ "type": "json_schema", "json_schema": { "schema": {
            "type": "object", "additionalProperties": { "type": "string" }
        } } }),
    ] {
        let upstream = upstream().await;
        provider(&upstream, false)
            .chat(request(format), CancellationToken::new())
            .await
            .unwrap();
        let sent = sent_body(&upstream).await;
        assert!(sent.get("output_config").is_none(), "{sent}");
        assert!(sent.get("response_format").is_none(), "{sent}");
    }
}
