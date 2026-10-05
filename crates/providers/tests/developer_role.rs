//! ADR 016: OpenAI's `developer` role reaches OpenAI verbatim, and is sent
//! as `system` by the same provider when the upstream has no such role.

use lumen_core::{ChatMessage, ChatProvider, ChatRequest, MessageContent};
use lumen_providers::OpenAiProvider;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn msg(role: &str, text: &str) -> ChatMessage {
    ChatMessage {
        role: role.to_owned(),
        content: Some(MessageContent::Text(text.to_owned())),
        name: None,
        extra: serde_json::Map::new(),
    }
}

fn request() -> ChatRequest {
    ChatRequest {
        model: "gpt-5".to_owned(),
        messages: vec![
            msg("developer", "be brief"),
            msg("system", "in French"),
            msg("user", "hi"),
        ],
        temperature: None,
        top_p: None,
        max_tokens: None,
        n: None,
        stop: None,
        stream: false,
        extra: serde_json::Map::new(),
    }
}

/// Send the request on both chat paths; return the roles each one sent.
async fn sent_roles(native: bool) -> Vec<Vec<String>> {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "c", "object": "chat.completion", "created": 1, "model": "gpt-5",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "ok" },
                "finish_reason": "stop"
            }]
        })))
        .mount(&upstream)
        .await;
    let provider = OpenAiProvider::new(
        reqwest::Client::new(),
        "openai",
        Some(upstream.uri()),
        Some("sk-test".to_owned()),
    )
    .with_native_developer_role(native);

    provider
        .chat(request(), CancellationToken::new())
        .await
        .unwrap();
    let _ = provider
        .chat_stream_bytes(request(), CancellationToken::new())
        .await
        .unwrap();

    upstream
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| {
            let sent: Value = serde_json::from_slice(&r.body).unwrap();
            sent["messages"]
                .as_array()
                .unwrap()
                .iter()
                .map(|m| m["role"].as_str().unwrap().to_owned())
                .collect()
        })
        .collect()
}

#[tokio::test]
async fn native_upstream_receives_developer_verbatim() {
    let roles = sent_roles(true).await;
    assert_eq!(roles.len(), 2);
    for r in roles {
        assert_eq!(r, ["developer", "system", "user"]);
    }
}

#[tokio::test]
async fn compatible_upstream_receives_developer_as_system() {
    let roles = sent_roles(false).await;
    assert_eq!(roles.len(), 2);
    for r in roles {
        assert_eq!(r, ["system", "system", "user"]);
    }
}
