//! Wiremock tests of the TypeSafe-family decision provider, one block per
//! kind: URL, auth, body shape, envelope, status mapping, cancellation.

use std::time::{Duration, Instant};

use lumen_core::decisions::format::{parse, Format};
use lumen_core::{DecisionProvider, DecisionRequest, ProviderError};
use lumen_providers::decisions::family::FamilyDecisionProvider;
use lumen_providers::http::build_client;
use serde_json::json;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{body_json, header, header_exists, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const KEY: &str = "family-key-do-not-leak";

fn ts_req() -> DecisionRequest {
    let mut req = parse(br#"{"model":"client","state":"s","questions":{"q":{"type":"noul","instructions":"i"}},"x":1}"#,
                        Some(Format::TypeSafe)).unwrap().1;
    "pplx-decider-v1.1-27b".clone_into(&mut req.model);
    req
}

fn answers() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "model": "m", "answers": {"q": {"type": "noul", "noul": 0.7}},
        "usage": {"input_tokens": 9, "output_tokens": 1}
    }))
}

#[tokio::test]
async fn typesafe_default_path_bearer_and_forwarded_fields() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .and(header("authorization", format!("Bearer {KEY}").as_str()))
        .and(body_json(
            json!({"model": "pplx-decider-v1.1-27b", "state": "s",
                              "questions": {"q": {"type": "noul", "instructions": "i"}}, "x": 1}),
        ))
        .respond_with(answers())
        .expect(1)
        .mount(&mock)
        .await;
    let p = FamilyDecisionProvider::typesafe(
        build_client(),
        "typesafe",
        Some(mock.uri()),
        None,
        true,
        Some(KEY.into()),
    );
    let resp = p.decide(ts_req(), CancellationToken::new()).await.unwrap();
    assert_eq!(resp.usage.unwrap().input_tokens, 9);
}

#[tokio::test]
async fn typesafe_decisions_path_and_stripped_fields() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/decisions/v1/systemone"))
        .and(body_json(
            json!({"model": "pplx-decider-v1.1-27b", "state": "s",
                              "questions": {"q": {"type": "noul", "instructions": "i"}}}),
        ))
        .respond_with(answers())
        .expect(1)
        .mount(&mock)
        .await;
    let p = FamilyDecisionProvider::typesafe(
        build_client(),
        "liquid",
        Some(mock.uri()),
        Some("/decisions/v1/systemone".into()),
        false,
        Some(KEY.into()),
    );
    p.decide(ts_req(), CancellationToken::new()).await.unwrap();
}

#[tokio::test]
async fn perplexity_posts_to_v1_decisions_and_strips_unknown_fields() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/decisions"))
        .and(header_exists("authorization"))
        .and(body_json(
            json!({"model": "pplx-decider-v1.1-27b", "state": "s",
                              "questions": {"q": {"type": "noul", "instructions": "i"}}}),
        ))
        .respond_with(answers())
        .expect(1)
        .mount(&mock)
        .await;
    let p =
        FamilyDecisionProvider::perplexity(build_client(), "pplx", mock.uri(), Some(KEY.into()));
    p.decide(ts_req(), CancellationToken::new()).await.unwrap();
}

#[tokio::test]
async fn ollama_is_keyless_and_puts_raw_base64_images_top_level() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .and(body_json(json!({"model": "clef", "state": "look",
            "questions": {"q0": {"type": "noul", "instructions": "i"}}, "images": ["QUJD"]})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "model": "clef", "answers": {"q0": {"type": "noul", "noul": 0.5}}})))
        .expect(1)
        .mount(&mock)
        .await;
    let mut req = parse(
        br#"{"model":"x","input":[{"role":"user","content":[{"type":"input_text","text":"look"},
        {"type":"input_image","image_url":"data:image/png;base64,QUJD"}]}],
        "questions":[{"type":"predicate","instructions":"i"}]}"#,
        None,
    )
    .unwrap()
    .1;
    "clef".clone_into(&mut req.model);
    let p = FamilyDecisionProvider::ollama(build_client(), "ollama", mock.uri(), None);
    let resp = p.decide(req, CancellationToken::new()).await.unwrap();
    assert!(
        resp.usage.is_none(),
        "missing usage stays None for the handler to estimate"
    );
}

#[tokio::test]
async fn ollama_connection_refused_is_unavailable_and_retryable() {
    let p =
        FamilyDecisionProvider::ollama(build_client(), "ollama", "http://127.0.0.1:9".into(), None);
    let err = p
        .decide(ts_req(), CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(err, ProviderError::Unavailable { .. }), "{err:?}");
    assert!(err.is_retryable());
}

#[tokio::test]
async fn cloudflare_runs_the_model_under_the_account_root() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/client/v4/accounts/acct/ai/run/@cf/cloudflare/clef"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": {"model": "clef", "answers": {"q": {"type": "noul", "noul": 0.1}}},
            "success": true, "errors": [], "messages": []})))
        .expect(1)
        .mount(&mock)
        .await;
    let base = format!("{}/client/v4/accounts/acct/ai/v1", mock.uri());
    let p = FamilyDecisionProvider::cloudflare(build_client(), "cf", base, Some(KEY.into()));
    let mut req = ts_req();
    "clef".clone_into(&mut req.model);
    assert_eq!(
        p.decide(req, CancellationToken::new()).await.unwrap().model,
        "clef"
    );
}

#[tokio::test]
async fn status_mapping() {
    for (status, retry_after, check) in [
        (
            429u16,
            Some("7"),
            (|e: &ProviderError| matches!(e, ProviderError::RateLimited { retry_after: Some(d), .. } if *d == Duration::from_secs(7)))
                as fn(&ProviderError) -> bool,
        ),
        (529, None, |e| {
            matches!(
                e,
                ProviderError::Upstream {
                    status: 529,
                    retryable: true,
                    ..
                }
            )
        }),
        (504, None, |e| {
            matches!(
                e,
                ProviderError::Upstream {
                    status: 504,
                    retryable: true,
                    ..
                }
            )
        }),
        (413, None, |e| {
            matches!(
                e,
                ProviderError::Upstream {
                    status: 413,
                    retryable: false,
                    ..
                }
            )
        }),
        (422, None, |e| {
            matches!(
                e,
                ProviderError::Upstream {
                    status: 422,
                    retryable: false,
                    ..
                }
            )
        }),
    ] {
        let mock = MockServer::start().await;
        let mut template = ResponseTemplate::new(status);
        if let Some(r) = retry_after {
            template = template.insert_header("retry-after", r);
        }
        Mock::given(method("POST"))
            .respond_with(template)
            .mount(&mock)
            .await;
        let p = FamilyDecisionProvider::perplexity(
            build_client(),
            "pplx",
            mock.uri(),
            Some(KEY.into()),
        );
        let err = p
            .decide(ts_req(), CancellationToken::new())
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
        .respond_with(answers().set_delay(Duration::from_secs(30)))
        .mount(&mock)
        .await;
    let p =
        FamilyDecisionProvider::perplexity(build_client(), "pplx", mock.uri(), Some(KEY.into()));
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        trigger.cancel();
    });
    let started = Instant::now();
    let err = p.decide(ts_req(), cancel).await.unwrap_err();
    assert!(matches!(err, ProviderError::Cancelled));
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn debug_never_leaks_the_key() {
    let p = FamilyDecisionProvider::perplexity(
        build_client(),
        "pplx",
        "https://api.perplexity.ai".into(),
        Some(KEY.into()),
    );
    let printed = format!("{p:?}");
    assert!(!printed.contains(KEY) && printed.contains("<redacted>"));
}

fn typesafe_at(uri: String) -> FamilyDecisionProvider {
    FamilyDecisionProvider::typesafe(
        build_client(),
        "typesafe",
        Some(uri),
        None,
        true,
        Some(KEY.into()),
    )
}

#[tokio::test]
async fn missing_usage_is_none_not_zero() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"model": "jev", "answers": {"q": {"type": "noul", "noul": 0.7}}}),
        ))
        .mount(&mock)
        .await;
    let resp = typesafe_at(mock.uri())
        .decide(ts_req(), CancellationToken::new())
        .await
        .unwrap();
    assert!(resp.usage.is_none());
}

#[tokio::test]
async fn malformed_usage_does_not_fail_a_billed_answer() {
    // A billed answer must never become a 502 over its usage block: the
    // counts are parsed leniently and the gateway estimates instead.
    for usage in [
        json!({"input_tokens": null}),
        json!({"input_tokens": 318.0}),
        json!("n/a"),
    ] {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"model": "jev", "answers": {"q": {"type": "noul", "noul": 0.7}}, "usage": usage})),
            )
            .mount(&mock)
            .await;
        let resp = typesafe_at(mock.uri())
            .decide(ts_req(), CancellationToken::new())
            .await
            .unwrap_or_else(|e| panic!("{usage}: {e:?}"));
        assert!(resp.usage.is_none(), "{usage}");
    }
}

#[tokio::test]
async fn typesafe_malformed_body_is_a_translation_error() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"answers": {}})))
        .mount(&mock)
        .await;
    let err = typesafe_at(mock.uri())
        .decide(ts_req(), CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(err, ProviderError::Translation(_)), "{err:?}");
}
