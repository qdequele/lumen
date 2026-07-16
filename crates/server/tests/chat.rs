//! End-to-end HTTP tests for `POST /v1/chat/completions`: non-streaming routing
//! and passthrough, Anthropic translation, streaming SSE (chunks + `[DONE]`),
//! client-disconnect cancellation, and routing/validation errors.

// Building a fixture SSE body is clearest with format!; the style lints don't
// earn their keep in test scaffolding.
#![allow(clippy::format_collect)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use figment::{
    providers::{Format, Toml},
    Figment,
};
use lumen_core::Capability;
use lumen_providers::{http, ModelSpec, ProviderKind, ProviderSpec, Registry};
use lumen_server::config::Config;
use serde_json::{json, Value};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const LIMIT: usize = 10 * 1024 * 1024;

/// Parse a TOML snippet into a full [`Config`] (defaults fill the rest).
fn config_from(toml: &str) -> Config {
    Figment::new()
        .merge(Toml::string(toml))
        .extract::<Config>()
        .expect("valid test config")
}

/// Spawn the full app from a config. `spawn_state` applies
/// `config.server.body_limit` onto the state (for the `LM-1002` message) and
/// into the body-size-limit layer from the same value.
async fn spawn(config: &Config) -> String {
    let registry = Arc::new(
        Registry::build(
            config.provider_specs(),
            http::build_client(),
            std::time::Duration::from_secs(300),
        )
        .expect("registry builds"),
    );
    let state = common::base_state(registry);
    common::spawn_state(state, config.server.body_limit).await
}

/// One OpenAI-kind provider (chat + an embed-only model) pointed at `upstream`.
fn openai_registry(upstream: &str) -> Arc<Registry> {
    let specs = vec![ProviderSpec {
        name: "openai".to_owned(),
        kind: ProviderKind::Openai,
        api_key: Some("sk-test-xxx".to_owned()),
        base_url: Some(upstream.to_owned()),
        api_version: None,
        strict: false,
        connect_timeout_ms: None,
        models: vec![
            ModelSpec {
                id: "gpt".to_owned(),
                upstream_id: "gpt-4o-2024-08-06".to_owned(),
                capabilities: vec![Capability::Chat],
                modalities: vec!["text".to_owned()],
            },
            ModelSpec {
                id: "embed-only".to_owned(),
                upstream_id: "text-embedding-3-small".to_owned(),
                capabilities: vec![Capability::Embed],
                modalities: vec!["text".to_owned()],
            },
        ],
    }];
    Arc::new(
        Registry::build(
            specs,
            http::build_client(),
            std::time::Duration::from_secs(300),
        )
        .expect("registry builds"),
    )
}

fn anthropic_registry(upstream: &str) -> Arc<Registry> {
    let specs = vec![ProviderSpec {
        name: "anthropic".to_owned(),
        kind: ProviderKind::Anthropic,
        api_key: Some("sk-ant-test".to_owned()),
        base_url: Some(upstream.to_owned()),
        api_version: None,
        strict: false,
        connect_timeout_ms: None,
        models: vec![ModelSpec {
            id: "claude".to_owned(),
            upstream_id: "claude-3-5-sonnet".to_owned(),
            capabilities: vec![Capability::Chat],
            modalities: vec!["text".to_owned()],
        }],
    }];
    Arc::new(
        Registry::build(
            specs,
            http::build_client(),
            std::time::Duration::from_secs(300),
        )
        .expect("registry builds"),
    )
}

fn google_registry(upstream: &str) -> Arc<Registry> {
    let specs = vec![ProviderSpec {
        name: "google".to_owned(),
        kind: ProviderKind::Google,
        api_key: Some("goog-test".to_owned()),
        base_url: Some(upstream.to_owned()),
        api_version: None,
        strict: false,
        connect_timeout_ms: None,
        models: vec![ModelSpec {
            id: "gemini".to_owned(),
            upstream_id: "gemini-2.0-flash".to_owned(),
            capabilities: vec![Capability::Chat],
            modalities: vec!["text".to_owned()],
        }],
    }];
    Arc::new(
        Registry::build(
            specs,
            http::build_client(),
            std::time::Duration::from_secs(300),
        )
        .expect("registry builds"),
    )
}

/// Vision-capable variant of [`anthropic_registry`] (issue #12 tests).
fn anthropic_vision_registry(upstream: &str) -> Arc<Registry> {
    let specs = vec![ProviderSpec {
        name: "anthropic".to_owned(),
        kind: ProviderKind::Anthropic,
        api_key: Some("sk-ant-test".to_owned()),
        base_url: Some(upstream.to_owned()),
        api_version: None,
        strict: false,
        connect_timeout_ms: None,
        models: vec![ModelSpec {
            id: "claude".to_owned(),
            upstream_id: "claude-3-5-sonnet".to_owned(),
            capabilities: vec![Capability::Chat],
            modalities: vec!["text".to_owned(), "image".to_owned()],
        }],
    }];
    Arc::new(
        Registry::build(
            specs,
            http::build_client(),
            std::time::Duration::from_secs(300),
        )
        .expect("registry builds"),
    )
}

/// Vision-capable variant of [`google_registry`] (issue #12 tests).
fn google_vision_registry(upstream: &str) -> Arc<Registry> {
    let specs = vec![ProviderSpec {
        name: "google".to_owned(),
        kind: ProviderKind::Google,
        api_key: Some("goog-test".to_owned()),
        base_url: Some(upstream.to_owned()),
        api_version: None,
        strict: false,
        connect_timeout_ms: None,
        models: vec![ModelSpec {
            id: "gemini".to_owned(),
            upstream_id: "gemini-2.0-flash".to_owned(),
            capabilities: vec![Capability::Chat],
            modalities: vec!["text".to_owned(), "image".to_owned()],
        }],
    }];
    Arc::new(
        Registry::build(
            specs,
            http::build_client(),
            std::time::Duration::from_secs(300),
        )
        .expect("registry builds"),
    )
}

/// Vision-capable Cohere registry (issue #73 tests): one Command-A-Vision
/// model declaring the `image` modality.
fn cohere_vision_registry(upstream: &str) -> Arc<Registry> {
    let specs = vec![ProviderSpec {
        name: "cohere".to_owned(),
        kind: ProviderKind::Cohere,
        api_key: Some("co-test".to_owned()),
        base_url: Some(upstream.to_owned()),
        strict: false,
        connect_timeout_ms: None,
        api_version: None,
        models: vec![ModelSpec {
            id: "command-a-vision".to_owned(),
            upstream_id: "command-a-vision-07-2025".to_owned(),
            capabilities: vec![Capability::Chat],
            modalities: vec!["text".to_owned(), "image".to_owned()],
        }],
    }];
    Arc::new(
        Registry::build(
            specs,
            http::build_client(),
            std::time::Duration::from_secs(300),
        )
        .expect("registry builds"),
    )
}

fn openai_chat_body(model: &str) -> Value {
    json!({
        "object": "chat.completion",
        "id": "chatcmpl-1",
        "created": 1,
        "model": model,
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": "hi there" },
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 8, "completion_tokens": 2, "total_tokens": 10 }
    })
}

#[tokio::test]
async fn openai_compatible_kind_routes_through_the_openai_path() {
    // A new OpenAI-compatible kind (Groq) pointed at a mock via base_url must
    // route exactly like the OpenAI kind - proving the shared provider wiring.
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(openai_chat_body("llama-3.3-70b")))
        .mount(&upstream)
        .await;

    let specs = vec![ProviderSpec {
        name: "groq".to_owned(),
        kind: ProviderKind::Groq,
        api_key: Some("gsk-test".to_owned()),
        base_url: Some(upstream.uri()), // override the built-in api.groq.com default
        api_version: None,
        strict: false,
        connect_timeout_ms: None,
        models: vec![ModelSpec {
            id: "fast".to_owned(),
            upstream_id: "llama-3.3-70b".to_owned(),
            capabilities: vec![Capability::Chat],
            modalities: vec!["text".to_owned()],
        }],
    }];
    let registry = Arc::new(
        Registry::build(
            specs,
            http::build_client(),
            std::time::Duration::from_secs(300),
        )
        .expect("registry builds"),
    );
    let base = common::spawn_with(registry, LIMIT).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({ "model": "fast", "messages": [{ "role": "user", "content": "hi" }] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["object"], "chat.completion");
    // Alias resolved to the upstream id, just like the OpenAI kind.
    let requests = upstream.received_requests().await.unwrap();
    let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(sent["model"], "llama-3.3-70b");
}

#[tokio::test]
async fn non_streaming_happy_path_resolves_alias_and_returns_openai_shape() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(openai_chat_body("gpt-4o-2024-08-06")),
        )
        .mount(&upstream)
        .await;

    let base = common::spawn_with(openai_registry(&upstream.uri()), LIMIT).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({ "model": "gpt", "messages": [{ "role": "user", "content": "hi" }] }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["object"], "chat.completion");
    assert_eq!(body["choices"][0]["message"]["content"], "hi there");
    assert_eq!(body["usage"]["total_tokens"], 10);

    // The client-facing id was resolved to the upstream id, stream forced false.
    let requests = upstream.received_requests().await.unwrap();
    let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(sent["model"], "gpt-4o-2024-08-06");
    assert_eq!(sent["stream"], false);
}

#[tokio::test]
async fn unknown_model_is_404_fg2001() {
    let upstream = MockServer::start().await;
    let base = common::spawn_with(openai_registry(&upstream.uri()), LIMIT).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({ "model": "nope", "messages": [{ "role": "user", "content": "x" }] }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 404);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "LM-2001");
}

#[tokio::test]
async fn embed_only_model_requested_for_chat_is_400_fg2002() {
    let upstream = MockServer::start().await;
    let base = common::spawn_with(openai_registry(&upstream.uri()), LIMIT).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({ "model": "embed-only", "messages": [{ "role": "user", "content": "x" }] }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "LM-2002");
}

#[tokio::test]
async fn image_to_a_non_vision_model_is_rejected_with_lm_2003() {
    // Upstream must never be called; mount nothing that would 200.
    let upstream = MockServer::start().await;
    // "gpt" declares default modalities (text only, see openai_registry), so
    // an image content part is rejected pre-flight.
    let base = common::spawn_with(openai_registry(&upstream.uri()), LIMIT).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({
            "model": "gpt",
            "messages": [{"role":"user","content":[
                {"type":"text","text":"hi"},
                {"type":"image_url","image_url":{"url":"https://example.com/x.png"}}
            ]}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "LM-2003");
    // The upstream was never contacted.
    assert!(upstream.received_requests().await.unwrap().is_empty());
}

/// OpenAI-family conformance: a model declared vision-capable (`modalities`
/// includes `"image"`) forwards the OpenAI content-parts array to the
/// upstream byte-for-byte (no re-shaping), and - since this mock upstream
/// reports no `usage` at all - the response still carries a non-zero,
/// honestly-labelled `estimated` token count rather than a silent zero
/// (ADR 003 addendum: the estimation fallback fires for a vision request;
/// the image part now contributes the flat per-image heuristic from
/// `lumen_core::tokens`, not 0 - see issue #9).
#[tokio::test]
async fn openai_family_forwards_image_parts_verbatim() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "c", "object": "chat.completion", "created": 0, "model": "gpt",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "a cat" },
                "finish_reason": "stop"
            }]
            // Deliberately no "usage" - exercises the estimation fallback.
        })))
        .mount(&upstream)
        .await;

    let cfg = format!(
        r#"
        [[providers]]
        name = "openai"
        kind = "openai"
        base_url = "{}"
        [[providers.models]]
        id = "gpt"
        capabilities = ["chat"]
        modalities = ["text", "image"]
        "#,
        upstream.uri()
    );
    let base = spawn(&config_from(&cfg)).await;

    let body = json!({
        "model": "gpt",
        "messages": [{"role":"user","content":[
            {"type":"text","text":"what is this?"},
            {"type":"image_url","image_url":{"url":"data:image/png;base64,AAAA"}}
        ]}]
    });
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // The upstream received the image part unchanged - no stripping, no
    // reshaping of the content-parts array.
    let reqs = upstream.received_requests().await.unwrap();
    let sent: Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_eq!(
        sent["messages"][0]["content"][1]["image_url"]["url"],
        "data:image/png;base64,AAAA"
    );
    assert_eq!(sent["messages"][0]["content"][0]["text"], "what is this?");

    // No upstream usage → the local estimator ran, flagged honestly, never a
    // silent zero. "what is this?" (13 bytes) => 4 text tokens + the 4-token
    // per-message overhead + the flat per-image heuristic (no `detail`, so
    // the default 765-token estimate, issue #9) = 773 - pinning this value
    // proves the image part is counted exactly once, at the documented
    // amount, not silently dropped back to 0.
    let got: Value = resp.json().await.unwrap();
    assert_eq!(got["usage"]["estimated"], true);
    assert_eq!(got["usage"]["prompt_tokens"], 773);
}

#[tokio::test]
async fn empty_messages_is_400_fg1001() {
    let upstream = MockServer::start().await;
    let base = common::spawn_with(openai_registry(&upstream.uri()), LIMIT).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({ "model": "gpt", "messages": [] }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "LM-1001");
}

#[tokio::test]
async fn upstream_5xx_propagates_as_502_fg3003() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&upstream)
        .await;
    let base = common::spawn_with(openai_registry(&upstream.uri()), LIMIT).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({ "model": "gpt", "messages": [{ "role": "user", "content": "x" }] }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 502);
    assert_eq!(
        resp.json::<Value>().await.unwrap()["error"]["code"],
        "LM-3003"
    );
}

#[tokio::test]
async fn anthropic_translation_round_trip() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "claude-3-5-sonnet",
            "content": [{ "type": "text", "text": "bonjour" }],
            "stop_reason": "end_turn",
            "usage": { "input_tokens": 12, "output_tokens": 3 }
        })))
        .mount(&upstream)
        .await;

    let base = common::spawn_with(anthropic_registry(&upstream.uri()), LIMIT).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({
            "model": "claude",
            "messages": [
                { "role": "system", "content": "be terse" },
                { "role": "user", "content": "salut" }
            ],
            "max_tokens": 100
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    // Anthropic response translated back to OpenAI shape.
    assert_eq!(body["object"], "chat.completion");
    assert_eq!(body["choices"][0]["message"]["content"], "bonjour");
    assert_eq!(body["choices"][0]["finish_reason"], "stop");
    assert_eq!(body["usage"]["prompt_tokens"], 12);
    assert_eq!(body["usage"]["completion_tokens"], 3);

    // Outgoing Anthropic request: system hoisted, only the user message remains.
    let requests = upstream.received_requests().await.unwrap();
    let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(sent["model"], "claude-3-5-sonnet");
    assert_eq!(sent["system"], "be terse");
    assert_eq!(sent["messages"].as_array().unwrap().len(), 1);
    assert_eq!(sent["messages"][0]["role"], "user");
    assert_eq!(sent["max_tokens"], 100);
}

#[tokio::test]
async fn google_gemini_translation_round_trip() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1beta/models/gemini-2.0-flash:generateContent"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "candidates": [{
                "content": { "role": "model", "parts": [{ "text": "salut" }] },
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 5,
                "candidatesTokenCount": 2,
                "totalTokenCount": 7
            }
        })))
        .mount(&upstream)
        .await;

    let base = common::spawn_with(google_registry(&upstream.uri()), LIMIT).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({
            "model": "gemini",
            "messages": [
                { "role": "system", "content": "sois bref" },
                { "role": "user", "content": "bonjour" }
            ]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["object"], "chat.completion");
    assert_eq!(body["choices"][0]["message"]["content"], "salut");
    assert_eq!(body["choices"][0]["finish_reason"], "stop");
    assert_eq!(body["usage"]["total_tokens"], 7);

    // Outgoing Gemini request: system hoisted, roles mapped, model in the path.
    let requests = upstream.received_requests().await.unwrap();
    let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(sent["systemInstruction"]["parts"][0]["text"], "sois bref");
    assert_eq!(sent["contents"][0]["role"], "user");
    assert_eq!(sent["contents"][0]["parts"][0]["text"], "bonjour");
}

/// Issue #12: an `anthropic-file:<file_id>` image source reaches Anthropic
/// as a `source: {type: "file", file_id}` block, not a `url`/`base64` one.
#[tokio::test]
async fn anthropic_file_id_forwards_as_a_file_source_block() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "claude-3-5-sonnet",
            "content": [{ "type": "text", "text": "a cat" }],
            "stop_reason": "end_turn",
            "usage": { "input_tokens": 12, "output_tokens": 3 }
        })))
        .mount(&upstream)
        .await;

    let base = common::spawn_with(anthropic_vision_registry(&upstream.uri()), LIMIT).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({
            "model": "claude",
            "messages": [{"role":"user","content":[
                {"type":"text","text":"what is this?"},
                {"type":"image_url","image_url":{"url":"anthropic-file:file_011CNvxvfvyGnGnDtjPtzY9J"}}
            ]}],
            "max_tokens": 100
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);

    let requests = upstream.received_requests().await.unwrap();
    let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
    let block = &sent["messages"][0]["content"][1];
    assert_eq!(block["type"], "image");
    assert_eq!(block["source"]["type"], "file");
    assert_eq!(block["source"]["file_id"], "file_011CNvxvfvyGnGnDtjPtzY9J");
    assert!(block["source"].get("data").is_none());
    assert!(block["source"].get("url").is_none());
}

/// Issue #12: a `gs://` GCS URI image source reaches Gemini as a
/// `fileData.fileUri` part, not an `inline_data` one.
#[tokio::test]
async fn gemini_gcs_uri_forwards_as_a_file_data_part() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1beta/models/gemini-2.0-flash:generateContent"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "candidates": [{
                "content": { "role": "model", "parts": [{ "text": "a cat" }] },
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 5,
                "candidatesTokenCount": 2,
                "totalTokenCount": 7
            }
        })))
        .mount(&upstream)
        .await;

    let base = common::spawn_with(google_vision_registry(&upstream.uri()), LIMIT).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({
            "model": "gemini",
            "messages": [{"role":"user","content":[
                {"type":"text","text":"what is this?"},
                {"type":"image_url","image_url":{"url":"gs://my-bucket/cat.png"}}
            ]}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);

    let requests = upstream.received_requests().await.unwrap();
    let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
    let part = &sent["contents"][0]["parts"][1];
    assert_eq!(part["file_data"]["file_uri"], "gs://my-bucket/cat.png");
    assert_eq!(part["file_data"]["mime_type"], "image/png");
    assert!(part.get("inline_data").is_none());
}

/// Issue #12 (review fix): a Gemini Files API URI is an `https://` URL, so it
/// satisfies `is_remote()` too. It must NOT be caught by the `LM-2004`
/// remote-URL pre-flight (Google declines fetchable URLs) - a provider-native
/// reference bound for its own provider passes through and reaches Gemini as
/// `fileData.fileUri`.
#[tokio::test]
async fn gemini_files_api_uri_forwards_as_a_file_data_part() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1beta/models/gemini-2.0-flash:generateContent"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "candidates": [{
                "content": { "role": "model", "parts": [{ "text": "a cat" }] },
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 5,
                "candidatesTokenCount": 2,
                "totalTokenCount": 7
            }
        })))
        .mount(&upstream)
        .await;

    let base = common::spawn_with(google_vision_registry(&upstream.uri()), LIMIT).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({
            "model": "gemini",
            "messages": [{"role":"user","content":[
                {"type":"text","text":"what is this?"},
                {"type":"image_url","image_url":{"url":"https://generativelanguage.googleapis.com/v1beta/files/abc-123"}}
            ]}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);

    let requests = upstream.received_requests().await.unwrap();
    let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
    let part = &sent["contents"][0]["parts"][1];
    assert_eq!(
        part["file_data"]["file_uri"],
        "https://generativelanguage.googleapis.com/v1beta/files/abc-123"
    );
    // No extension on a Files API URI: mime_type omitted, never guessed.
    assert!(part["file_data"].get("mime_type").is_none());
    assert!(part.get("inline_data").is_none());
}

/// Issue #12 (review fix): the same Files API URI bound for a non-Google
/// primary is `LM-2008` (provider-native source mismatch), NOT `LM-2004`
/// (generic remote URL) and NOT forwarded - the upstream is never contacted.
#[tokio::test]
async fn gemini_files_api_uri_sent_to_anthropic_is_400_lm_2008() {
    let upstream = MockServer::start().await;
    let base = common::spawn_with(anthropic_vision_registry(&upstream.uri()), LIMIT).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({
            "model": "claude",
            "messages": [{"role":"user","content":[
                {"type":"text","text":"what is this?"},
                {"type":"image_url","image_url":{"url":"https://generativelanguage.googleapis.com/v1beta/files/abc-123"}}
            ]}],
            "max_tokens": 100
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "LM-2008");
    assert_eq!(body["error"]["type"], "invalid_request");
    assert!(upstream.received_requests().await.unwrap().is_empty());
}

/// Issue #12: an Anthropic-native `file_id` sent to a provider that is not
/// Anthropic is an honest `LM-2008` client error, not a 502 - the upstream
/// is never contacted.
#[tokio::test]
async fn anthropic_file_id_sent_to_google_is_400_lm_2008() {
    let upstream = MockServer::start().await;
    let base = common::spawn_with(google_vision_registry(&upstream.uri()), LIMIT).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({
            "model": "gemini",
            "messages": [{"role":"user","content":[
                {"type":"text","text":"what is this?"},
                {"type":"image_url","image_url":{"url":"anthropic-file:file_011CNvxvfvyGnGnDtjPtzY9J"}}
            ]}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "LM-2008");
    assert_eq!(body["error"]["type"], "invalid_request");
    assert!(upstream.received_requests().await.unwrap().is_empty());
}

/// Issue #12: a Gemini-native `gs://` / Files API URI sent to a provider
/// that is not Google is an honest `LM-2008` client error, not a 502 - the
/// upstream is never contacted.
#[tokio::test]
async fn gemini_file_uri_sent_to_anthropic_is_400_lm_2008() {
    let upstream = MockServer::start().await;
    let base = common::spawn_with(anthropic_vision_registry(&upstream.uri()), LIMIT).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({
            "model": "claude",
            "messages": [{"role":"user","content":[
                {"type":"text","text":"what is this?"},
                {"type":"image_url","image_url":{"url":"gs://my-bucket/cat.png"}}
            ]}],
            "max_tokens": 100
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "LM-2008");
    assert_eq!(body["error"]["type"], "invalid_request");
    assert!(upstream.received_requests().await.unwrap().is_empty());
}

/// Issue #73: an inline `data:` URI image routed to a Cohere vision model
/// reaches `/v2/chat` as v2 content blocks (text + `image_url`), not
/// flattened text - and the upstream-reported usage stays authoritative
/// (ADR 003), never overwritten by the estimator.
#[tokio::test]
async fn cohere_forwards_image_parts_as_v2_content_blocks() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v2/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "chat_vis_1",
            "finish_reason": "COMPLETE",
            "message": {
                "role": "assistant",
                "content": [{ "type": "text", "text": "a cat" }]
            },
            "usage": { "tokens": { "input_tokens": 21, "output_tokens": 3 } }
        })))
        .mount(&upstream)
        .await;

    let base = common::spawn_with(cohere_vision_registry(&upstream.uri()), LIMIT).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({
            "model": "command-a-vision",
            "messages": [{"role":"user","content":[
                {"type":"text","text":"what is this?"},
                {"type":"image_url","image_url":{"url":"data:image/png;base64,AAAA"}}
            ]}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);

    let requests = upstream.received_requests().await.unwrap();
    let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
    let content = sent["messages"][0]["content"]
        .as_array()
        .expect("image-bearing message must be an array of v2 blocks");
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[0]["text"], "what is this?");
    assert_eq!(content[1]["type"], "image_url");
    assert_eq!(content[1]["image_url"]["url"], "data:image/png;base64,AAAA");

    // Upstream usage is authoritative: no `estimated` flag, counts verbatim.
    let got: Value = resp.json().await.unwrap();
    assert_eq!(got["usage"]["prompt_tokens"], 21);
    assert_eq!(got["usage"]["completion_tokens"], 3);
    assert!(got["usage"].get("estimated").is_none());
}

/// Issue #73: Cohere v2 fetches remote `http(s)` image URLs itself, so the
/// `LM-2004` pre-flight must NOT fire - the URL is forwarded verbatim inside
/// a v2 `image_url` block. This upstream reports no usage at all, so the
/// ADR 003 estimation fallback fires with the per-image heuristic (85 tokens
/// at detail low, 765 otherwise), honestly flagged - never a silent zero.
#[tokio::test]
async fn cohere_accepts_and_forwards_a_remote_image_url() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v2/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "chat_vis_2",
            "finish_reason": "COMPLETE",
            "message": {
                "role": "assistant",
                "content": [{ "type": "text", "text": "a cat" }]
            }
            // Deliberately no "usage" - exercises the estimation fallback.
        })))
        .mount(&upstream)
        .await;

    let base = common::spawn_with(cohere_vision_registry(&upstream.uri()), LIMIT).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({
            "model": "command-a-vision",
            "messages": [{"role":"user","content":[
                {"type":"text","text":"what is this?"},
                {"type":"image_url","image_url":{"url":"https://example.com/cat.png"}}
            ]}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);

    let requests = upstream.received_requests().await.unwrap();
    let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
    let block = &sent["messages"][0]["content"][1];
    assert_eq!(block["type"], "image_url");
    assert_eq!(block["image_url"]["url"], "https://example.com/cat.png");

    // No upstream usage: the local estimator ran, flagged honestly. Same
    // arithmetic as the OpenAI-family test above: 4 text tokens + 4 per-
    // message overhead + 765 (no `detail`) = 773.
    let got: Value = resp.json().await.unwrap();
    assert_eq!(got["usage"]["estimated"], true);
    assert_eq!(got["usage"]["prompt_tokens"], 773);
}

/// Issue #73: the provider-native reference forms stay honest 400s for the
/// cohere kind - an Anthropic Files API reference cannot be resolved by
/// Cohere, so it is `LM-2008` pre-flight, upstream never contacted.
#[tokio::test]
async fn anthropic_file_id_sent_to_cohere_is_400_lm_2008() {
    let upstream = MockServer::start().await;
    let base = common::spawn_with(cohere_vision_registry(&upstream.uri()), LIMIT).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({
            "model": "command-a-vision",
            "messages": [{"role":"user","content":[
                {"type":"text","text":"what is this?"},
                {"type":"image_url","image_url":{"url":"anthropic-file:file_011CNvxvfvyGnGnDtjPtzY9J"}}
            ]}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "LM-2008");
    assert!(upstream.received_requests().await.unwrap().is_empty());
}

/// Issue #73: same for a Gemini-native `gs://` reference routed to Cohere.
#[tokio::test]
async fn gemini_file_uri_sent_to_cohere_is_400_lm_2008() {
    let upstream = MockServer::start().await;
    let base = common::spawn_with(cohere_vision_registry(&upstream.uri()), LIMIT).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({
            "model": "command-a-vision",
            "messages": [{"role":"user","content":[
                {"type":"text","text":"what is this?"},
                {"type":"image_url","image_url":{"url":"gs://my-bucket/cat.png"}}
            ]}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "LM-2008");
    assert!(upstream.received_requests().await.unwrap().is_empty());
}

/// Build an upstream OpenAI-style SSE body of `n` chunk frames + `[DONE]`, with
/// deliberately compact, quirky spacing so a byte-identical assertion proves the
/// gateway forwarded verbatim rather than deserializing and re-serializing.
fn upstream_sse_body(n: usize) -> String {
    let frames: String = (0..n)
        .map(|i| {
            format!(
                "data: {{\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"tok{i}\"}}}}]}}\n\n"
            )
        })
        .collect();
    format!("{frames}data: [DONE]\n\n")
}

#[tokio::test]
async fn streaming_passthrough_is_byte_identical() {
    let upstream = MockServer::start().await;
    let body = upstream_sse_body(100);
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body.clone()),
        )
        .mount(&upstream)
        .await;

    let base = common::spawn_with(openai_registry(&upstream.uri()), LIMIT).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({
            "model": "gpt",
            "messages": [{ "role": "user", "content": "hi" }],
            "stream": true
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let ct = resp
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    assert!(ct.starts_with("text/event-stream"), "content-type: {ct}");

    // Zero-copy passthrough: the client receives the upstream body byte-for-byte
    // (all 100 chunks + [DONE]); any re-serialization would perturb formatting.
    let text = resp.text().await.unwrap();
    assert_eq!(text, body);
    assert_eq!(text.matches("chat.completion.chunk").count(), 100);
    assert!(text.ends_with("data: [DONE]\n\n"));

    // The gateway asked the upstream to stream with usage (ADR 003 hook).
    let requests = upstream.received_requests().await.unwrap();
    let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(sent["stream"], true);
    assert_eq!(sent["stream_options"]["include_usage"], true);
}

#[tokio::test]
async fn streaming_client_disconnect_does_not_hang_server() {
    let upstream = MockServer::start().await;
    // Upstream is slow to even produce the response; client cuts well before.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(upstream_sse_body(3))
                .set_delay(Duration::from_secs(3)),
        )
        .mount(&upstream)
        .await;

    let base = common::spawn_with(openai_registry(&upstream.uri()), LIMIT).await;

    // Client gives up after 200ms, well before the 3s upstream delay.
    let started = std::time::Instant::now();
    let result = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .timeout(Duration::from_millis(200))
        .json(&json!({
            "model": "gpt",
            "messages": [{ "role": "user", "content": "x" }],
            "stream": true
        }))
        .send()
        .await;
    assert!(result.is_err(), "client should have timed out");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "client aborted promptly, not after the 3s upstream delay"
    );

    // Server stays responsive afterwards (the cancelled request didn't wedge it).
    let health = reqwest::get(format!("{base}/health")).await.unwrap();
    assert_eq!(health.status(), 200);
}

/// A raw TCP "upstream" that streams SSE frames and signals when its connection
/// is closed by the peer (the gateway). Detects the FIN via a 0-byte read.
async fn spawn_abort_detecting_upstream() -> (String, tokio::sync::oneshot::Receiver<()>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();

    tokio::spawn(async move {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        // Drain the request (small, one read on loopback) before responding, so
        // we don't close/respond while the gateway is still writing its request.
        let mut req = [0u8; 4096];
        let _ = socket.read(&mut req).await;
        let (mut rd, mut wr) = socket.split();
        let head =
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";
        if wr.write_all(head.as_bytes()).await.is_err() {
            return;
        }
        let frame = "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"x\"}}]}\n\n";

        // Keep streaming frames while watching for the peer's FIN. When the
        // gateway drops its upstream connection (because the client
        // disconnected), the read side sees EOF and we signal.
        let write_loop = async {
            loop {
                if wr.write_all(frame.as_bytes()).await.is_err() {
                    break;
                }
                let _ = wr.flush().await;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        let read_eof = async {
            let mut buf = [0u8; 256];
            loop {
                match rd.read(&mut buf).await {
                    Ok(0) | Err(_) => break, // peer closed
                    Ok(_) => {}
                }
            }
        };
        tokio::select! {
            () = write_loop => {},
            () = read_eof => {},
        }
        let _ = tx.send(());
    });

    (format!("http://{addr}"), rx)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn streaming_client_disconnect_aborts_upstream_connection() {
    // Acceptance criterion 2: a client disconnect mid-stream must close the
    // upstream connection promptly (the LiteLLM #22805 lesson). This exercises
    // the drop-guard / bytes_stream-drop path that the timing-only test cannot.
    let (upstream_url, rx) = spawn_abort_detecting_upstream().await;
    let base = common::spawn_with(openai_registry(&upstream_url), LIMIT).await;

    let client = reqwest::Client::new();
    let mut resp = client
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({
            "model": "gpt",
            "messages": [{ "role": "user", "content": "x" }],
            "stream": true
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // Read the first streamed frame, then disconnect by dropping the response.
    let first = resp.chunk().await.unwrap();
    assert!(first.is_some(), "expected at least one streamed frame");
    drop(resp);

    // The upstream observes its connection torn down within the window.
    let closed = tokio::time::timeout(Duration::from_secs(3), rx).await;
    assert!(
        matches!(closed, Ok(Ok(()))),
        "upstream connection was not aborted after client disconnect"
    );
}

// ---- M4 finish: streaming translation + stream guards ----------------------

/// Anthropic SSE event fixture (text + tool_use), in upstream wire format.
fn anthropic_sse_body() -> String {
    [
        (
            "message_start",
            json!({
                "type": "message_start",
                "message": {
                    "id": "msg_str", "model": "claude-3-5-sonnet",
                    "usage": { "input_tokens": 12 }
                }
            }),
        ),
        (
            "content_block_start",
            json!({
                "type": "content_block_start", "index": 0,
                "content_block": { "type": "text", "text": "" }
            }),
        ),
        (
            "content_block_delta",
            json!({
                "type": "content_block_delta", "index": 0,
                "delta": { "type": "text_delta", "text": "Hel" }
            }),
        ),
        (
            "content_block_delta",
            json!({
                "type": "content_block_delta", "index": 0,
                "delta": { "type": "text_delta", "text": "lo" }
            }),
        ),
        (
            "content_block_stop",
            json!({ "type": "content_block_stop", "index": 0 }),
        ),
        (
            "content_block_start",
            json!({
                "type": "content_block_start", "index": 1,
                "content_block": { "type": "tool_use", "id": "toolu_9", "name": "lookup" }
            }),
        ),
        (
            "content_block_delta",
            json!({
                "type": "content_block_delta", "index": 1,
                "delta": { "type": "input_json_delta", "partial_json": "{\"q\":\"x\"}" }
            }),
        ),
        (
            "message_delta",
            json!({
                "type": "message_delta",
                "delta": { "stop_reason": "tool_use" },
                "usage": { "output_tokens": 9 }
            }),
        ),
        ("message_stop", json!({ "type": "message_stop" })),
    ]
    .iter()
    .map(|(name, data)| format!("event: {name}\ndata: {data}\n\n"))
    .collect()
}

/// Parse the `data:` payloads of an SSE body (excluding `[DONE]` and comments).
fn sse_data_frames(body: &str) -> Vec<Value> {
    body.split("\n\n")
        .filter_map(|frame| frame.strip_prefix("data: "))
        .filter(|data| *data != "[DONE]")
        .map(|data| serde_json::from_str(data).expect("frame is JSON"))
        .collect()
}

#[tokio::test]
async fn anthropic_streaming_translates_events_to_openai_chunks() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(anthropic_sse_body()),
        )
        .mount(&upstream)
        .await;

    let base = common::spawn_with(anthropic_registry(&upstream.uri()), LIMIT).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({
            "model": "claude",
            "messages": [{ "role": "user", "content": "hi" }],
            "stream": true
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();

    // Clean upstream termination: translated [DONE], and no LM-3010.
    assert!(text.ends_with("data: [DONE]\n\n"), "got: {text}");
    assert!(!text.contains("LM-3010"));

    let chunks = sse_data_frames(&text);
    // role, 2 text deltas, tool open, tool args, finish.
    assert_eq!(chunks.len(), 6, "got: {text}");
    assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
    assert_eq!(chunks[0]["id"], "msg_str");
    assert_eq!(chunks[0]["model"], "claude-3-5-sonnet");
    assert_eq!(chunks[1]["choices"][0]["delta"]["content"], "Hel");
    assert_eq!(chunks[2]["choices"][0]["delta"]["content"], "lo");
    let tool_open = &chunks[3]["choices"][0]["delta"]["tool_calls"][0];
    assert_eq!(tool_open["index"], 0);
    assert_eq!(tool_open["id"], "toolu_9");
    assert_eq!(tool_open["function"]["name"], "lookup");
    let tool_args = &chunks[4]["choices"][0]["delta"]["tool_calls"][0];
    assert_eq!(tool_args["function"]["arguments"], "{\"q\":\"x\"}");
    assert_eq!(chunks[5]["choices"][0]["finish_reason"], "tool_calls");
    // ADR 003: the final chunk carries full usage.
    assert_eq!(chunks[5]["usage"]["prompt_tokens"], 12);
    assert_eq!(chunks[5]["usage"]["completion_tokens"], 9);
    assert_eq!(chunks[5]["usage"]["total_tokens"], 21);

    // The upstream was asked to stream.
    let requests = upstream.received_requests().await.unwrap();
    let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(sent["stream"], true);
}

#[tokio::test]
async fn gemini_streaming_translates_fragments_to_openai_chunks() {
    let upstream = MockServer::start().await;
    let body = [
        json!({
            "candidates": [
                { "content": { "parts": [{ "text": "Bon" }], "role": "model" } }
            ]
        }),
        json!({
            "candidates": [{
                "content": { "parts": [{ "text": "jour" }], "role": "model" },
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 5, "candidatesTokenCount": 3, "totalTokenCount": 8
            }
        }),
    ]
    .iter()
    .map(|data| format!("data: {data}\n\n"))
    .collect::<String>();

    Mock::given(method("POST"))
        .and(path(
            "/v1beta/models/gemini-2.0-flash:streamGenerateContent",
        ))
        .and(query_param("alt", "sse"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&upstream)
        .await;

    let base = common::spawn_with(google_registry(&upstream.uri()), LIMIT).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({
            "model": "gemini",
            "messages": [{ "role": "user", "content": "salut" }],
            "stream": true
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();

    assert!(text.ends_with("data: [DONE]\n\n"), "got: {text}");
    assert!(!text.contains("LM-3010"));

    let chunks = sse_data_frames(&text);
    // role, 2 text deltas, finish.
    assert_eq!(chunks.len(), 4, "got: {text}");
    assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
    assert_eq!(chunks[1]["choices"][0]["delta"]["content"], "Bon");
    assert_eq!(chunks[2]["choices"][0]["delta"]["content"], "jour");
    assert_eq!(chunks[3]["choices"][0]["finish_reason"], "stop");
    assert_eq!(chunks[3]["usage"]["total_tokens"], 8);
}

#[tokio::test]
async fn gemini_tool_calling_non_streaming_round_trip() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1beta/models/gemini-2.0-flash:generateContent"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{
                        "functionCall": { "name": "get_weather", "args": { "city": "Paris" } }
                    }]
                },
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 9, "candidatesTokenCount": 5, "totalTokenCount": 14
            }
        })))
        .mount(&upstream)
        .await;

    let base = common::spawn_with(google_registry(&upstream.uri()), LIMIT).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({
            "model": "gemini",
            "messages": [{ "role": "user", "content": "weather in Paris?" }],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "description": "Weather lookup",
                    "parameters": {
                        "type": "object",
                        "properties": { "city": { "type": "string" } },
                        "required": ["city"]
                    }
                }
            }],
            "tool_choice": "auto"
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    // Downstream: an OpenAI tool-call message with null content.
    assert!(body["choices"][0]["message"]["content"].is_null());
    let call = &body["choices"][0]["message"]["tool_calls"][0];
    assert_eq!(call["type"], "function");
    assert_eq!(call["function"]["name"], "get_weather");
    assert_eq!(call["function"]["arguments"], "{\"city\":\"Paris\"}");
    assert_eq!(body["choices"][0]["finish_reason"], "tool_calls");

    // Upstream: OpenAI tools mapped to Gemini functionDeclarations + toolConfig.
    let requests = upstream.received_requests().await.unwrap();
    let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
    let decl = &sent["tools"][0]["functionDeclarations"][0];
    assert_eq!(decl["name"], "get_weather");
    assert_eq!(decl["parameters"]["properties"]["city"]["type"], "string");
    assert_eq!(sent["toolConfig"]["functionCallingConfig"]["mode"], "AUTO");
}

#[tokio::test]
async fn gemini_streaming_function_call_translates_to_tool_calls() {
    let upstream = MockServer::start().await;
    let body = [json!({
        "candidates": [{
            "content": {
                "role": "model",
                "parts": [{
                    "functionCall": { "name": "get_weather", "args": { "city": "Paris" } }
                }]
            },
            "finishReason": "STOP"
        }],
        "usageMetadata": {
            "promptTokenCount": 9, "candidatesTokenCount": 5, "totalTokenCount": 14
        }
    })]
    .iter()
    .map(|data| format!("data: {data}\n\n"))
    .collect::<String>();

    Mock::given(method("POST"))
        .and(path(
            "/v1beta/models/gemini-2.0-flash:streamGenerateContent",
        ))
        .and(query_param("alt", "sse"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&upstream)
        .await;

    let base = common::spawn_with(google_registry(&upstream.uri()), LIMIT).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({
            "model": "gemini",
            "messages": [{ "role": "user", "content": "weather in Paris?" }],
            "tools": [{
                "type": "function",
                "function": { "name": "get_weather" }
            }],
            "stream": true
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();

    assert!(text.ends_with("data: [DONE]\n\n"), "got: {text}");
    let chunks = sse_data_frames(&text);
    // role, tool-call delta, finish.
    assert_eq!(chunks.len(), 3, "got: {text}");
    assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
    let call = &chunks[1]["choices"][0]["delta"]["tool_calls"][0];
    assert_eq!(call["index"], 0);
    assert_eq!(call["function"]["name"], "get_weather");
    assert_eq!(call["function"]["arguments"], "{\"city\":\"Paris\"}");
    assert_eq!(chunks[2]["choices"][0]["finish_reason"], "tool_calls");
}

#[tokio::test]
async fn upstream_stream_without_done_yields_fg3010_error_frame() {
    let upstream = MockServer::start().await;
    // Two valid chunks, then the body just ends - no `data: [DONE]`.
    let truncated: String = upstream_sse_body(2)
        .strip_suffix("data: [DONE]\n\n")
        .unwrap()
        .to_owned();
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(truncated),
        )
        .mount(&upstream)
        .await;

    let base = common::spawn_with(openai_registry(&upstream.uri()), LIMIT).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({
            "model": "gpt",
            "messages": [{ "role": "user", "content": "hi" }],
            "stream": true
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();

    // Both real chunks were forwarded, then a terminal LM-3010 error frame
    // (criterion 5) - and the stream ended cleanly, no hang.
    assert_eq!(text.matches("chat.completion.chunk").count(), 2);
    assert!(!text.contains("data: [DONE]"));
    let frames = sse_data_frames(&text);
    let last = frames.last().unwrap();
    assert_eq!(last["error"]["code"], "LM-3010");
    assert_eq!(last["error"]["type"], "upstream_error");
}

#[tokio::test]
async fn first_token_timeout_non_streaming_is_504_fg3011() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(openai_chat_body("gpt-4o-2024-08-06"))
                .set_delay(Duration::from_secs(5)),
        )
        .mount(&upstream)
        .await;

    let guards = lumen_server::StreamGuards {
        first_token_timeout: Duration::from_millis(150),
        heartbeat_interval: Duration::from_secs(15),
    };
    let base = common::spawn_with_guards(openai_registry(&upstream.uri()), LIMIT, guards).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({ "model": "gpt", "messages": [{ "role": "user", "content": "hi" }] }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 504);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "LM-3011");
    assert_eq!(body["error"]["type"], "upstream_error");
}

#[tokio::test]
async fn first_token_timeout_streaming_before_headers_is_504_fg3011() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(upstream_sse_body(1))
                .set_delay(Duration::from_secs(5)),
        )
        .mount(&upstream)
        .await;

    let guards = lumen_server::StreamGuards {
        first_token_timeout: Duration::from_millis(150),
        heartbeat_interval: Duration::from_secs(15),
    };
    let base = common::spawn_with_guards(openai_registry(&upstream.uri()), LIMIT, guards).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({
            "model": "gpt",
            "messages": [{ "role": "user", "content": "hi" }],
            "stream": true
        }))
        .send()
        .await
        .unwrap();

    // The upstream never answered within the window: the stream never started,
    // so this is an honest 504 JSON envelope rather than an SSE error frame.
    assert_eq!(resp.status(), 504);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "LM-3011");
}

#[tokio::test]
async fn oversized_body_returns_lm_1002_envelope() {
    let upstream = wiremock::MockServer::start().await;
    let cfg = format!(
        r#"
        [server]
        body_limit = 256

        [[providers]]
        name = "openai"
        kind = "openai"
        base_url = "{}"
        [[providers.models]]
        id = "gpt"
        capabilities = ["chat"]
        "#,
        upstream.uri()
    );
    let base = spawn(&config_from(&cfg)).await;

    let big = "x".repeat(4096);
    let body = serde_json::json!({ "model": "gpt", "messages": [{"role":"user","content": big}] });
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 413);
    let json: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(json["error"]["code"], "LM-1002");
}

/// A syntactically-invalid but under-limit body must still map to `LM-1001`
/// (400) - the `LM-1002` middleware only rewrites bare `413`s, so this must
/// not regress.
#[tokio::test]
async fn malformed_json_body_is_still_lm_1001() {
    let upstream = MockServer::start().await;
    let base = common::spawn_with(openai_registry(&upstream.uri()), LIMIT).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .header("content-type", "application/json")
        .body("{ not valid json")
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "LM-1001");
}
