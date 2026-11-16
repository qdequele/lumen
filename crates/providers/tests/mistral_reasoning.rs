//! Mistral reasoning models answer with `content` as an array of chunks
//! (`thinking`, `text`). The provider hands clients OpenAI's shape instead: a
//! string `content` and the thinking in `reasoning_content`, streamed or not.

use futures::StreamExt;
use lumen_core::{ChatCompletion, ChatMessage, ChatProvider, ChatRequest, MessageContent};
use lumen_providers::MistralProvider;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A real `mistral-large-4` answer to "test", shortened.
const REASONING_BODY: &str = r#"{"id":"517dc24d","object":"chat.completion","created":1794834439,"model":"mistral-large-4","choices":[{"index":0,"message":{"role":"assistant","content":[{"type":"thinking","closed":true,"thinking":[{"text":"The user said \"test\".","type":"text"}]},{"type":"text","text":"Hello! I'm here and working."}],"tool_calls":null},"finish_reason":"stop"}],"usage":{"prompt_tokens":16,"completion_tokens":87,"total_tokens":103,"completion_tokens_details":{"reasoning_tokens":69}}}"#;

const REASONING_STREAM: &str = concat!(
    "data: {\"id\":\"s\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"mistral-large-4\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":[{\"type\":\"thinking\",\"thinking\":[{\"type\":\"text\",\"text\":\"The user\"}]}]},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"s\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"mistral-large-4\",\"choices\":[{\"index\":0,\"delta\":{\"content\":[{\"type\":\"thinking\",\"thinking\":[{\"type\":\"text\",\"text\":\" said test.\"}]}]},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"s\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"mistral-large-4\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello!\"},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"s\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"mistral-large-4\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":16,\"completion_tokens\":12,\"total_tokens\":28}}\n\n",
    "data: [DONE]\n\n",
);

fn request(messages: Value) -> ChatRequest {
    ChatRequest {
        model: "mistral-large-4".to_owned(),
        messages: serde_json::from_value::<Vec<ChatMessage>>(messages).unwrap(),
        temperature: None,
        top_p: None,
        max_tokens: None,
        n: None,
        stop: None,
        stream: false,
        extra: serde_json::Map::new(),
    }
}

fn user_test() -> ChatRequest {
    request(json!([{ "role": "user", "content": "test" }]))
}

async fn upstream(body: &'static str, content_type: &str) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, content_type))
        .mount(&server)
        .await;
    server
}

fn provider(server: &MockServer) -> MistralProvider {
    MistralProvider::new(
        reqwest::Client::new(),
        "mistral",
        Some(server.uri()),
        Some("sk-test".to_owned()),
    )
}

fn completion_json(completion: ChatCompletion) -> Value {
    match completion {
        ChatCompletion::Passthrough(p) => serde_json::from_slice(&p.into_body()).unwrap(),
        ChatCompletion::Typed(t) => serde_json::to_value(t).unwrap(),
    }
}

#[tokio::test]
async fn a_reasoning_completion_has_string_content_and_reasoning_content() {
    let server = upstream(REASONING_BODY, "application/json").await;
    let completion = provider(&server)
        .chat_completion(user_test(), CancellationToken::new())
        .await
        .unwrap();
    assert!(
        matches!(completion, ChatCompletion::Passthrough(_)),
        "the rewritten body still carries upstream usage, so it is forwarded"
    );
    let body = completion_json(completion);
    let message = &body["choices"][0]["message"];
    assert_eq!(message["content"], "Hello! I'm here and working.");
    assert_eq!(message["reasoning_content"], "The user said \"test\".");
    assert_eq!(
        body["usage"]["completion_tokens_details"]["reasoning_tokens"],
        69
    );
}

#[tokio::test]
async fn the_typed_chat_path_reads_the_text_only() {
    let server = upstream(REASONING_BODY, "application/json").await;
    let response = provider(&server)
        .chat(user_test(), CancellationToken::new())
        .await
        .unwrap();
    let message = &response.choices[0].message;
    assert_eq!(
        message.content,
        Some(MessageContent::Text(
            "Hello! I'm here and working.".to_owned()
        ))
    );
    assert_eq!(
        message.extra["reasoning_content"],
        "The user said \"test\"."
    );
}

#[tokio::test]
async fn a_reasoning_stream_has_string_deltas() {
    let server = upstream(REASONING_STREAM, "text/event-stream").await;
    let bytes: Vec<u8> = provider(&server)
        .chat_stream_bytes(user_test(), CancellationToken::new())
        .await
        .unwrap()
        .map(|chunk| chunk.unwrap().to_vec())
        .concat()
        .await;
    let text = String::from_utf8(bytes).unwrap();
    assert!(text.ends_with("data: [DONE]\n\n"));

    let mut content = String::new();
    let mut reasoning = String::new();
    let mut saw_usage = false;
    for frame in text.split("\n\n").filter(|f| !f.is_empty()) {
        let data = frame.strip_prefix("data: ").unwrap();
        if data == "[DONE]" {
            continue;
        }
        let chunk: Value = serde_json::from_str(data).unwrap();
        let delta = &chunk["choices"][0]["delta"];
        assert!(
            delta.get("content").is_none_or(Value::is_string),
            "every delta content is a string: {delta}"
        );
        content.push_str(delta["content"].as_str().unwrap_or_default());
        reasoning.push_str(delta["reasoning_content"].as_str().unwrap_or_default());
        saw_usage |= chunk.get("usage").is_some();
    }
    assert_eq!(content, "Hello!");
    assert_eq!(reasoning, "The user said test.");
    assert!(saw_usage, "the final usage frame is kept");
}

#[tokio::test]
async fn replayed_reasoning_content_is_not_sent_to_mistral() {
    let server = upstream(REASONING_BODY, "application/json").await;
    let req = request(json!([
        { "role": "user", "content": "test" },
        { "role": "assistant", "content": "Hello!", "reasoning_content": "The user said test." },
        { "role": "user", "content": "again" }
    ]));
    provider(&server)
        .chat_completion(req, CancellationToken::new())
        .await
        .unwrap();
    let received = server.received_requests().await.unwrap();
    let sent: Value = serde_json::from_slice(&received[0].body).unwrap();
    assert!(sent["messages"]
        .as_array()
        .unwrap()
        .iter()
        .all(|m| m.get("reasoning_content").is_none()));
    assert_eq!(sent["messages"][1]["content"], "Hello!");
}
