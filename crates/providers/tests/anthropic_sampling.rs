//! Anthropic Messages API request-shape guards for OpenAI fields the API
//! does not take as-is: `temperature` together with `top_p` (current Claude
//! models accept one or the other, never both), and `n > 1` (no equivalent).

use lumen_core::{ChatMessage, ChatProvider, ChatRequest, MessageContent, ProviderError};
use lumen_providers::AnthropicProvider;
use serde_json::{json, Value};
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
        stream: false,
        extra: serde_json::Map::new(),
    }
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
            "content": [{ "type": "text", "text": "ok" }],
            "stop_reason": "end_turn",
            "usage": { "input_tokens": 3, "output_tokens": 1 }
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
    let requests = upstream.received_requests().await.unwrap();
    serde_json::from_slice(&requests[requests.len() - 1].body).unwrap()
}

/// With both sampling knobs set, only `temperature` reaches the wire: the
/// pair is rejected upstream by current Claude models.
#[tokio::test]
async fn top_p_is_dropped_when_temperature_is_also_set() {
    let upstream = upstream().await;
    let mut req = request();
    req.temperature = Some(0.5);
    req.top_p = Some(0.9);
    provider(&upstream, false)
        .chat(req, CancellationToken::new())
        .await
        .unwrap();

    let body = sent_body(&upstream).await;
    assert_eq!(body["temperature"], 0.5);
    assert!(body.get("top_p").is_none(), "{body}");
}

/// Either knob alone is forwarded untouched.
#[tokio::test]
async fn a_single_sampling_knob_is_forwarded() {
    let upstream = upstream().await;
    let mut req = request();
    req.top_p = Some(0.25);
    provider(&upstream, false)
        .chat(req, CancellationToken::new())
        .await
        .unwrap();

    let body = sent_body(&upstream).await;
    assert_eq!(body["top_p"], 0.25);
    assert!(body.get("temperature").is_none(), "{body}");
}

/// `n > 1` cannot be honoured: a strict provider rejects it before any
/// upstream call, on both paths; a lenient one answers with one completion.
#[tokio::test]
async fn n_above_one_is_rejected_in_strict_mode_and_dropped_otherwise() {
    let upstream = upstream().await;
    let mut req = request();
    req.n = Some(3);

    let strict = provider(&upstream, true);
    let err = strict
        .chat(req.clone(), CancellationToken::new())
        .await
        .unwrap_err();
    assert!(
        matches!(&err, ProviderError::UnsupportedField { provider, field }
            if provider == "anthropic-test" && field == "n"),
        "{err:?}"
    );
    let Err(err) = strict
        .chat_stream(req.clone(), CancellationToken::new())
        .await
    else {
        panic!("strict streaming must reject n > 1");
    };
    assert!(
        matches!(err, ProviderError::UnsupportedField { .. }),
        "{err:?}"
    );
    assert!(upstream.received_requests().await.unwrap().is_empty());

    // `n = 1` is the default and is never rejected, even when strict.
    let mut one = request();
    one.n = Some(1);
    strict.chat(one, CancellationToken::new()).await.unwrap();

    let out = provider(&upstream, false)
        .chat(req, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(out.choices.len(), 1);
}
