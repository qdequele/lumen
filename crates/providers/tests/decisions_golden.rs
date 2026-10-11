//! Golden Jev wire bytes (ADR 017, D7 and D9). Run once with
//! `LUMEN_BLESS=1` on the pre-refactor code to write the fixtures; every
//! later run compares the bytes the code sends against them.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use lumen_core::{RerankDocument, RerankProvider, RerankRequest};
use serde_json::json;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/decisions")
        .join(name)
}

/// Compare `bytes` with the fixture, or write it when `LUMEN_BLESS=1`.
fn golden(name: &str, bytes: &[u8]) {
    let path = fixture(name);
    if std::env::var_os("LUMEN_BLESS").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, bytes).unwrap();
        return;
    }
    let want = std::fs::read(&path).unwrap_or_else(|_| panic!("missing fixture {name}"));
    assert_eq!(
        String::from_utf8_lossy(bytes),
        String::from_utf8_lossy(&want),
        "{name} drifted from the golden bytes"
    );
}

/// Records every request body and answers every question generically.
#[derive(Clone, Default)]
struct Recorder(Arc<Mutex<Vec<Vec<u8>>>>);

impl Respond for Recorder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        self.0.lock().unwrap().push(request.body.clone());
        let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        let mut answers = serde_json::Map::new();
        for (id, q) in body["questions"].as_object().unwrap() {
            let answer = match q["type"].as_str().unwrap() {
                "noul" => json!({"type": "noul", "noul": 0.5}),
                "score" => json!({"type": "score", "score": 1.0, "legend": {"0": "a"},
                                  "probabilities": {"0": 1.0}, "confidence": 0.9}),
                _ => {
                    let first = q["criteria"]
                        .as_object()
                        .unwrap()
                        .keys()
                        .next()
                        .unwrap()
                        .clone();
                    json!({"type": "choice", "choice": first.clone(),
                           "probabilities": {first: 1.0}, "confidence": 0.9})
                }
            };
            answers.insert(id.clone(), answer);
        }
        ResponseTemplate::new(200).set_body_json(json!({
            "model": "jev-1.13.0", "answers": answers,
            "usage": {"input_tokens": 10, "output_tokens": 1}
        }))
    }
}

async fn recorder() -> (MockServer, Recorder) {
    let mock = MockServer::start().await;
    let rec = Recorder::default();
    Mock::given(method("POST"))
        .respond_with(rec.clone())
        .mount(&mock)
        .await;
    (mock, rec)
}

fn rerank_request() -> RerankRequest {
    RerankRequest {
        model: "jev-latest".to_owned(),
        query: "which case \"cites\" Smith v. Jones?".to_owned(),
        documents: vec![
            RerankDocument::Text("Smith v. Jones (1999) held that caf\u{e9}s...".to_owned()),
            RerankDocument::Text("Unrelated text with a\nnewline".to_owned()),
            RerankDocument::Text("Third doc".to_owned()),
        ],
        rank_fields: None,
        top_n: None,
        return_documents: false,
    }
}

const PASSTHROUGH: &str = r#"{"model":"jev","state":{"zeta":"café\n","alpha":[1,2]},"questions":{"urgent":{"type":"noul","instructions":{"q":"urgent?"},"criteria":{"true":"yes"}},"team":{"type":"choice","instructions":"Which team?","criteria":{"tech":"Bugs","billing":null}},"mood":{"type":"score","instructions":"Mood?","criteria":["calm","angry"]}},"future_flag":true}"#;

#[tokio::test]
async fn passthrough_request_bytes() {
    let (mock, rec) = recorder().await;
    let provider = lumen_providers::FamilyDecisionProvider::typesafe(
        lumen_providers::http::build_client(),
        "typesafe",
        Some(mock.uri()),
        None,
        true,
        Some("k".into()),
    );
    let (_, mut req) = lumen_core::decisions::format::parse(PASSTHROUGH.as_bytes()).unwrap();
    "jev-latest".clone_into(&mut req.model);
    lumen_core::DecisionProvider::decide(&provider, req, CancellationToken::new())
        .await
        .unwrap();
    golden("passthrough_request.json", &rec.0.lock().unwrap()[0]);
}

async fn rerank_bytes(strategy: lumen_providers::decisions::rerank::RerankStrategy, name: &str) {
    use lumen_providers::decisions::rerank::{DecisionRerankProvider, RerankTemplate};
    let (mock, rec) = recorder().await;
    let inner = Arc::new(lumen_providers::FamilyDecisionProvider::typesafe(
        lumen_providers::http::build_client(),
        "typesafe",
        Some(mock.uri()),
        None,
        true,
        Some("k".into()),
    ));
    let provider = DecisionRerankProvider::new(
        inner,
        "typesafe",
        Arc::new(RerankTemplate {
            context: Some("US case law".to_owned()),
            strategy,
        }),
    );
    provider
        .rerank(rerank_request(), CancellationToken::new())
        .await
        .unwrap();
    golden(name, &rec.0.lock().unwrap()[0]);
}

#[tokio::test]
async fn rerank_noul_request_bytes() {
    rerank_bytes(
        lumen_providers::decisions::rerank::RerankTemplate::default().strategy,
        "rerank_noul_request.json",
    )
    .await;
}

#[tokio::test]
async fn rerank_score_request_bytes() {
    rerank_bytes(
        lumen_providers::decisions::rerank::RerankStrategy::Score {
            instructions: "How relevant?".to_owned(),
            levels: vec!["none".to_owned(), "some".to_owned(), "exact".to_owned()],
        },
        "rerank_score_request.json",
    )
    .await;
}

#[tokio::test]
async fn rerank_composite_request_bytes() {
    use lumen_providers::decisions::rerank::{CompositeQuestion, RerankStrategy};
    rerank_bytes(
        RerankStrategy::Composite {
            questions: vec![
                CompositeQuestion {
                    instructions: "On topic?".into(),
                    criteria_true: "y".into(),
                    criteria_false: "n".into(),
                    weight: 0.7,
                },
                CompositeQuestion {
                    instructions: "Recent?".into(),
                    criteria_true: "y2".into(),
                    criteria_false: "n2".into(),
                    weight: 0.3,
                },
            ],
        },
        "rerank_composite_request.json",
    )
    .await;
}

#[tokio::test]
async fn rerank_choice_request_bytes() {
    rerank_bytes(
        lumen_providers::decisions::rerank::RerankStrategy::Choice {
            instructions: "Which document best answers?".to_owned(),
        },
        "rerank_choice_request.json",
    )
    .await;
}
