//! Wiremock tests of the OpenAI decision provider: URL, auth, body shape,
//! refusal passthrough, status mapping, cancellation, Debug redaction.

use std::time::{Duration, Instant};

use lumen_core::decisions::format::{parse, Format};
use lumen_core::{Answer, DecisionProvider, DecisionRequest, ProviderError};
use lumen_providers::decisions::openai::OpenAiDecisionProvider;
use lumen_providers::http::build_client;
use serde_json::json;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{body_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const KEY: &str = "openai-key-do-not-leak";

fn ts_req() -> DecisionRequest {
    let mut req = parse(
        br#"{"model":"client","state":{"ticket":"help"},"questions":{
        "urgent":{"type":"noul","instructions":"Urgent?","criteria":{"true":"time-sensitive","false":"can wait"}},
        "only":{"type":"noul","criteria":{"true":"mentions money"}},
        "team":{"type":"choice","instructions":{"q":"team"},"criteria":{"tech":"Bugs","billing":null}},
        "mood":{"type":"score","instructions":"Mood?","criteria":["calm","angry"]}},"future":1}"#,
        Some(Format::TypeSafe),
    )
    .unwrap()
    .1;
    "gpt-6-luna".clone_into(&mut req.model);
    req
}

fn one_q_req() -> DecisionRequest {
    let mut req = parse(
        br#"{"model":"client","state":"s","questions":{"q":{"type":"noul","instructions":"i"}}}"#,
        Some(Format::TypeSafe),
    )
    .unwrap()
    .1;
    "gpt-6-luna".clone_into(&mut req.model);
    req
}

fn four_answers() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "model": "gpt-6-luna",
        "answers": [
            {"type": "predicate", "name": "urgent", "probability": 0.8},
            {"type": "predicate", "name": "only", "probability": 0.1},
            {"type": "choice", "name": "team", "choice": "tech",
             "probabilities": [{"value": "tech", "probability": 0.7}, {"value": "billing", "probability": 0.3}],
             "confidence": 0.7},
            {"type": "score", "name": "mood", "score": 0.4,
             "probabilities": [{"value": 0, "probability": 0.6}, {"value": 1, "probability": 0.4}]}
        ],
        "usage": {"input_tokens": 9, "output_tokens": 1}
    }))
}

fn provider(mock: &MockServer) -> OpenAiDecisionProvider {
    OpenAiDecisionProvider::new(
        build_client(),
        "openai",
        Some(format!("{}/v1", mock.uri())),
        Some(KEY.into()),
    )
}

#[tokio::test]
async fn posts_the_translated_body_to_v1_decisions_with_bearer() {
    let mock = MockServer::start().await;
    let expected = format!(
        "{}{}{}{}{}",
        r#"{"model":"gpt-6-luna","input":"{\"ticket\":\"help\"}","questions":["#,
        r#"{"type":"predicate","name":"urgent","instructions":"Urgent?\nAnswer true if: time-sensitive. Answer false if: can wait."},"#,
        r#"{"type":"predicate","name":"only","instructions":"Answer true if: mentions money."},"#,
        r#"{"type":"choice","name":"team","instructions":"{\"q\":\"team\"}","choices":[{"value":"tech","description":"Bugs"},{"value":"billing"}]},"#,
        r#"{"type":"score","name":"mood","instructions":"Mood?","levels":[{"label":"calm"},{"label":"angry"}]}]}"#
    );
    let expected: serde_json::Value = serde_json::from_str(&expected).unwrap();
    Mock::given(method("POST"))
        .and(path("/v1/decisions"))
        .and(header("authorization", format!("Bearer {KEY}").as_str()))
        .and(body_json(expected))
        .respond_with(four_answers())
        .expect(1)
        .mount(&mock)
        .await;
    let resp = provider(&mock)
        .decide(ts_req(), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(resp.answers.len(), 4);
    assert_eq!(resp.usage.unwrap().input_tokens, 9);
}

#[tokio::test]
async fn refusal_passes_through() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/decisions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "model": "gpt-6-luna",
            "answers": [{"type": "refusal", "name": "q"}]
        })))
        .mount(&mock)
        .await;
    let resp = provider(&mock)
        .decide(one_q_req(), CancellationToken::new())
        .await
        .unwrap();
    assert!(matches!(resp.answers[0], Answer::Refusal));
}

#[tokio::test]
async fn status_mapping() {
    type Check = fn(&ProviderError) -> bool;
    let rows: [(u16, Option<&str>, &str, Check); 3] = [
        (
            429,
            Some("7"),
            "{}",
            |e| matches!(e, ProviderError::RateLimited { retry_after: Some(d), .. } if *d == Duration::from_secs(7)),
        ),
        (500, None, "{}", |e| {
            matches!(
                e,
                ProviderError::Upstream {
                    status: 500,
                    retryable: true,
                    ..
                }
            )
        }),
        (
            400,
            None,
            r#"{"error":{"code":"context_length_exceeded"}}"#,
            |e| matches!(e, ProviderError::ContextLengthExceeded { .. }),
        ),
    ];
    for (status, retry_after, body, check) in rows {
        let mock = MockServer::start().await;
        let mut template = ResponseTemplate::new(status).set_body_string(body);
        if let Some(r) = retry_after {
            template = template.insert_header("retry-after", r);
        }
        Mock::given(method("POST"))
            .respond_with(template)
            .mount(&mock)
            .await;
        let err = provider(&mock)
            .decide(one_q_req(), CancellationToken::new())
            .await
            .unwrap_err();
        assert!(check(&err), "{status}: {err:?}");
        assert!(!format!("{err:?} {err}").contains(KEY));
    }
}

#[tokio::test]
async fn cancellation_aborts_the_upstream_call() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(four_answers().set_delay(Duration::from_secs(30)))
        .mount(&mock)
        .await;
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        trigger.cancel();
    });
    let started = Instant::now();
    let err = provider(&mock)
        .decide(one_q_req(), cancel)
        .await
        .unwrap_err();
    assert!(matches!(err, ProviderError::Cancelled));
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn debug_never_leaks_the_key() {
    let p = OpenAiDecisionProvider::new(build_client(), "openai", None, Some(KEY.into()));
    let printed = format!("{p:?}");
    assert!(!printed.contains(KEY) && printed.contains("<redacted>"));
}
