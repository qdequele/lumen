//! An upstream 4xx is diagnosable from the log: the provider's own error
//! message is logged (bounded, credentials redacted) next to the LM-3003
//! `request failed` line, and is never returned to the client.

mod common;

use std::io::Write;
use std::sync::{Arc, Mutex};

use lumen_core::Capability;
use lumen_providers::{http, ModelSpec, ProviderKind, ProviderSpec, Registry};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The provider key configured for the test upstream; it must never be logged.
const PROVIDER_KEY: &str = "sk-ant-api03-DO-NOT-LOG-THIS-KEY";

#[derive(Clone)]
struct BufMakeWriter(Arc<Mutex<Vec<u8>>>);

struct BufGuard(Arc<Mutex<Vec<u8>>>);

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for BufMakeWriter {
    type Writer = BufGuard;
    fn make_writer(&'a self) -> Self::Writer {
        BufGuard(self.0.clone())
    }
}

impl Write for BufGuard {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("log buffer poisoned")
            .extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn registry(kind: ProviderKind, upstream: &str) -> Arc<Registry> {
    let specs = vec![ProviderSpec {
        name: kind.as_str().to_owned(),
        kind,
        api_key: Some(PROVIDER_KEY.to_owned()),
        base_url: Some(upstream.to_owned()),
        api_version: None,
        strict: false,
        connect_timeout_ms: None,
        models: vec![ModelSpec {
            id: "claude".to_owned(),
            upstream_id: "claude-sonnet-5-5".to_owned(),
            capabilities: vec![Capability::Chat],
            modalities: vec!["text".to_owned()],
            release_date: None,
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

/// Capture WARN+ events on this thread. The tests run on a single-threaded
/// runtime, so the server task (spawned on it) logs through this subscriber.
fn capture_logs() -> (Arc<Mutex<Vec<u8>>>, tracing::subscriber::DefaultGuard) {
    let buffer = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_writer(BufMakeWriter(buffer.clone()))
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .finish();
    let guard = tracing::subscriber::set_default(subscriber);
    (buffer, guard)
}

#[tokio::test(flavor = "current_thread")]
async fn upstream_400_message_is_logged_redacted_and_not_returned() {
    let (buffer, _guard) = capture_logs();

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "type": "error",
            "error": {
                "type": "invalid_request_error",
                "message": format!("messages.0.role: Input should be 'user' or 'assistant' (key {PROVIDER_KEY})")
            }
        })))
        .mount(&upstream)
        .await;
    let base = common::spawn_with(
        registry(ProviderKind::Anthropic, &upstream.uri()),
        1024 * 1024,
    )
    .await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({
            "model": "claude",
            "messages": [{ "role": "user", "content": "a private prompt" }]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 502);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "LM-3003");
    // The upstream's message stays out of the client response.
    assert!(
        !body.to_string().contains("Input should be"),
        "upstream detail leaked to the client: {body}"
    );

    let logs = String::from_utf8(buffer.lock().unwrap().clone()).unwrap();
    assert!(logs.contains("upstream returned an error"), "{logs}");
    assert!(
        logs.contains("messages.0.role: Input should be 'user' or 'assistant'"),
        "upstream message missing from the log: {logs}"
    );
    assert!(logs.contains("status=400"), "{logs}");
    assert!(logs.contains("LM-3003"), "{logs}");
    assert!(!logs.contains(PROVIDER_KEY), "provider key leaked: {logs}");
    assert!(!logs.contains("a private prompt"), "prompt leaked: {logs}");
}

/// pydantic-based hosts (vLLM here) quote the offending input inside their
/// error message: the logged detail is cut before that echo, so the prompt
/// never reaches the log.
#[tokio::test(flavor = "current_thread")]
async fn upstream_error_echoing_the_request_never_logs_the_prompt() {
    let (buffer, _guard) = capture_logs();

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "object": "error",
            "message": "1 validation error for ChatCompletionRequest: messages.0: \
                        Input tag 'foo' found, {'type': 'union_tag_invalid', \
                        'input': {'role': 'foo', 'content': 'a private prompt'}}",
            "type": "BadRequestError",
            "code": 400
        })))
        .mount(&upstream)
        .await;
    let base = common::spawn_with(registry(ProviderKind::Vllm, &upstream.uri()), 1024 * 1024).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({
            "model": "claude",
            "messages": [{ "role": "foo", "content": "a private prompt" }]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 502);

    let logs = String::from_utf8(buffer.lock().unwrap().clone()).unwrap();
    assert!(
        logs.contains(
            "1 validation error for ChatCompletionRequest: messages.0: Input tag 'foo' found"
        ),
        "upstream message missing from the log: {logs}"
    );
    assert!(!logs.contains("a private prompt"), "prompt leaked: {logs}");
}
