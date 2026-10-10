//! Privacy of `POST /v1/decisions` and `/v1/systemone` (spec 12): input,
//! questions and `safety_identifier` are request content and never reach a
//! log line, in either format, on success, on a rejected request, on a
//! skipped incompatible target, or through the deprecated alias. The
//! provider key never does either.
//!
//! Its own test binary: the server runs on tokio worker threads, which only
//! a *global* subscriber sees (`tracing::subscriber::with_default` is
//! thread-local), and a global subscriber can be installed once per process.

mod common;

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use figment::providers::{Format, Toml};
use figment::Figment;
use lumen_providers::{http, Registry};
use lumen_server::config::Config;
use lumen_server::pricing::CostTable;
use lumen_server::resilience::ResilienceRuntime;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const KEY: &str = "priv-test-key-do-not-leak";
const MARKERS: [&str; 3] = ["SECRET-INPUT", "SECRET-QUESTION", "SECRET-SAFETY"];

/// A `MakeWriter` that appends everything into a shared buffer, so the test
/// can inspect exactly what was logged.
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

fn config(typesafe: &str, luna: &str, decider: &str) -> Config {
    let toml = format!(
        r#"
        [[providers]]
        name = "typesafe"
        kind = "typesafe"
        base_url = "{typesafe}"
        api_key_env = "LUMEN_TEST_PRIVACY_KEY_UNUSED"
        [[providers.models]]
        id = "jev"
        upstream_id = "jev-latest"
        capabilities = ["decisions"]

        [[providers]]
        name = "openai"
        kind = "openai"
        base_url = "{luna}/v1"
        api_key_env = "LUMEN_TEST_PRIVACY_KEY_UNUSED"
        [[providers.models]]
        id = "luna"
        upstream_id = "gpt-6-luna"
        capabilities = ["decisions"]
        modalities = ["text", "image"]

        [[providers]]
        name = "perplexity"
        kind = "perplexity"
        base_url = "{decider}"
        api_key_env = "LUMEN_TEST_PRIVACY_KEY_UNUSED"
        [[providers.models]]
        id = "decider"
        upstream_id = "decider-1"
        capabilities = ["decisions"]

        [[virtual_models]]
        id = "luna-first"
        capability = "decisions"
        strategy = "fallback"
        targets = [{{ model = "luna" }}, {{ model = "decider" }}]
        "#
    );
    Figment::new()
        .merge(Toml::string(&toml))
        .extract::<Config>()
        .expect("valid test config")
}

async fn post(base: &str, route: &str, body: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{base}{route}"))
        .header("content-type", "application/json")
        .body(body.to_owned())
        .send()
        .await
        .expect("send")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decisions_content_never_reaches_the_logs() {
    let buffer = Arc::new(Mutex::new(Vec::new()));
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .with_writer(BufMakeWriter(buffer.clone()))
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .finish(),
    )
    .expect("the only global subscriber of this binary");

    let typesafe = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"model":"jev-latest","answers":{"q":{"type":"noul","noul":0.9}},"usage":{"input_tokens":10,"output_tokens":1}}"#,
            "application/json",
        ))
        .mount(&typesafe)
        .await;
    let luna = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/decisions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"model":"gpt-6-luna","answers":[{"type":"predicate","name":null,"probability":0.7}],"usage":{"input_tokens":9,"output_tokens":1,"total_tokens":10}}"#,
            "application/json",
        ))
        .mount(&luna)
        .await;
    let decider = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/decisions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"model":"decider-1","answers":{"c":{"type":"choice","choice":"only","probabilities":{"only":1.0}}},"usage":{"input_tokens":8,"output_tokens":1}}"#,
            "application/json",
        ))
        .mount(&decider)
        .await;

    let config = config(&typesafe.uri(), &luna.uri(), &decider.uri());
    let mut specs = config.provider_specs();
    for spec in &mut specs {
        spec.api_key = Some(KEY.to_owned());
    }
    let registry = Arc::new(
        Registry::build(specs, http::build_client(), Duration::from_secs(300))
            .expect("registry builds"),
    );
    let state = common::base_state(registry)
        .with_pricing(CostTable::from_config(&config))
        .with_resilience(Arc::new(ResilienceRuntime::from_config(&config, None)));
    let base = common::spawn_state(state, 10 * 1024 * 1024).await;

    let typesafe_ok = r#"{"model":"jev","state":{"text":"SECRET-INPUT"},"questions":{"q":{"type":"noul","instructions":"SECRET-QUESTION"}}}"#;
    let openai_ok = r#"{"model":"luna","input":[{"role":"user","content":[{"type":"input_text","text":"SECRET-INPUT"},{"type":"input_image","image_url":"data:image/png;base64,AA"}]}],"questions":[{"type":"predicate","instructions":"SECRET-QUESTION"}],"safety_identifier":"SECRET-SAFETY"}"#;
    let openai_invalid = r#"{"model":"luna","input":"SECRET-INPUT","questions":[{"type":"predicate","instructions":"SECRET-QUESTION"},{"type":"essay","instructions":"SECRET-QUESTION"}],"safety_identifier":"SECRET-SAFETY"}"#;
    let typesafe_invalid = r#"{"model":"jev","state":"SECRET-INPUT","questions":{"q":{"type":"choice","instructions":"SECRET-QUESTION"}}}"#;
    // A choice of one option: OpenAI is skipped, Perplexity answers.
    let skipped = r#"{"model":"luna-first","state":"SECRET-INPUT","questions":{"c":{"type":"choice","instructions":"SECRET-QUESTION","criteria":{"only":null}}}}"#;

    let cases = [
        ("/v1/decisions", typesafe_ok, 200),
        ("/v1/decisions", openai_ok, 200),
        ("/v1/decisions", openai_invalid, 400),
        ("/v1/decisions", typesafe_invalid, 400),
        ("/v1/decisions", skipped, 200),
        ("/v1/systemone", typesafe_ok, 200),
        ("/v1/systemone", openai_ok, 400),
    ];
    for (route, body, status) in cases {
        let resp = post(&base, route, body).await;
        assert_eq!(resp.status(), status, "{route} {body}");
        let text = resp.text().await.expect("body");
        assert!(!text.contains(KEY), "{text}");
    }
    assert_eq!(
        luna.received_requests().await.expect("recorded").len(),
        1,
        "only the compatible OpenAI-format success reached OpenAI"
    );

    let logs = String::from_utf8(buffer.lock().expect("log buffer").clone()).expect("utf8 logs");
    assert!(!logs.is_empty(), "the subscriber captured nothing");
    assert!(
        logs.contains("deprecated route used"),
        "the deprecation warning is logged"
    );
    assert!(
        logs.contains("skipping an incompatible decisions target"),
        "the skip is logged at debug"
    );
    for marker in MARKERS {
        assert!(!logs.contains(marker), "{marker} leaked into the logs");
    }
    assert!(!logs.contains(KEY), "the provider key leaked into the logs");
}
