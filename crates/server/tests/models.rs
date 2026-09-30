//! End-to-end HTTP tests for `GET /v1/models` and `GET /v1/models/{id}`: the
//! list reflects only the operator's configuration (no upstream
//! introspection) and reports each model's capabilities, including
//! multi-capability models; retrieve serves the same per-model object from
//! the same snapshot, and an unknown id is a 404 `LM-2001`.

mod common;

use std::sync::Arc;

use lumen_core::Capability;
use lumen_providers::{http, ModelSpec, ProviderKind, ProviderSpec, Registry};
use serde_json::Value;

fn registry() -> Arc<Registry> {
    let specs = vec![
        ProviderSpec {
            name: "cohere".to_owned(),
            kind: ProviderKind::Cohere,
            api_key: Some("sk-test-xxx".to_owned()),
            base_url: None,
            api_version: None,
            strict: false,
            connect_timeout_ms: None,
            models: vec![ModelSpec {
                id: "multi".to_owned(),
                upstream_id: "embed-v4.0".to_owned(),
                // A single Cohere model configured for BOTH embed and rerank.
                capabilities: vec![Capability::Embed, Capability::Rerank],
                modalities: vec!["text".to_owned()],
                rerank_template: None,
                release_date: None,
            }],
        },
        ProviderSpec {
            name: "openai".to_owned(),
            kind: ProviderKind::Openai,
            api_key: Some("sk-test-xxx".to_owned()),
            base_url: None,
            api_version: None,
            strict: false,
            connect_timeout_ms: None,
            models: vec![
                ModelSpec {
                    id: "gpt".to_owned(),
                    upstream_id: "gpt-4o".to_owned(),
                    capabilities: vec![Capability::Chat],
                    modalities: vec!["text".to_owned()],
                    rerank_template: None,
                    release_date: Some("2024-05-13".parse().unwrap()),
                },
                ModelSpec {
                    // A slash-containing id (HF-style), legal in config: the
                    // retrieve route must match it across path segments.
                    id: "mistralai/mistral-7b".to_owned(),
                    upstream_id: "mistralai/Mistral-7B-Instruct-v0.3".to_owned(),
                    capabilities: vec![Capability::Chat],
                    modalities: vec!["text".to_owned()],
                    rerank_template: None,
                    release_date: None,
                },
            ],
        },
    ];
    Arc::new(
        Registry::build(
            specs,
            http::build_client(),
            std::time::Duration::from_secs(300),
        )
        .expect("registry builds"),
    )
}

const LIMIT: usize = 10 * 1024 * 1024;

#[tokio::test]
async fn lists_configured_models_with_capabilities() {
    let base = common::spawn_with(registry(), LIMIT).await;

    let resp = reqwest::get(format!("{base}/v1/models")).await.unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();

    assert_eq!(body["object"], "list");
    let data = body["data"].as_array().unwrap();
    assert_eq!(data.len(), 3);

    let multi = data.iter().find(|m| m["id"] == "multi").unwrap();
    assert_eq!(multi["object"], "model");
    assert_eq!(multi["owned_by"], "cohere");
    let caps: Vec<&str> = multi["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap())
        .collect();
    // A Cohere embed+rerank model appears with both (acceptance criterion 3).
    assert!(caps.contains(&"embed"));
    assert!(caps.contains(&"rerank"));

    let gpt = data.iter().find(|m| m["id"] == "gpt").unwrap();
    assert_eq!(gpt["owned_by"], "openai");
    assert_eq!(gpt["capabilities"][0], "chat");
}

#[tokio::test]
async fn retrieve_known_model_matches_its_list_entry() {
    let base = common::spawn_with(registry(), LIMIT).await;

    // The list entry is the reference shape: retrieve must return the exact
    // same per-model object (issue #67 acceptance criterion 1).
    let list: Value = reqwest::get(format!("{base}/v1/models"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let list_entry = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "multi")
        .unwrap()
        .clone();

    let resp = reqwest::get(format!("{base}/v1/models/multi"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let retrieved: Value = resp.json().await.unwrap();

    assert_eq!(retrieved, list_entry);
    // Belt and braces on the OpenAI-shape fields.
    assert_eq!(retrieved["object"], "model");
    assert_eq!(retrieved["owned_by"], "cohere");
}

#[tokio::test]
async fn retrieve_slash_id_model_matches_its_list_entry() {
    let base = common::spawn_with(registry(), LIMIT).await;

    let list: Value = reqwest::get(format!("{base}/v1/models"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let list_entry = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "mistralai/mistral-7b")
        .unwrap()
        .clone();

    // Raw slash in the path (not percent-encoded), the way OpenAI-style
    // clients send HF-style ids.
    let resp = reqwest::get(format!("{base}/v1/models/mistralai/mistral-7b"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let retrieved: Value = resp.json().await.unwrap();

    assert_eq!(retrieved, list_entry);
    assert_eq!(retrieved["id"], "mistralai/mistral-7b");
    assert_eq!(retrieved["object"], "model");

    // The multi-segment route must not shadow the literal list route.
    let list_again = reqwest::get(format!("{base}/v1/models")).await.unwrap();
    assert_eq!(list_again.status(), 200);
    let body: Value = list_again.json().await.unwrap();
    assert_eq!(body["object"], "list");
}

#[tokio::test]
async fn retrieve_unknown_slash_path_is_404_lm2001() {
    let base = common::spawn_with(registry(), LIMIT).await;

    let resp = reqwest::get(format!("{base}/v1/models/no-such/model"))
        .await
        .unwrap();
    // A route miss would be an empty-body 404; the envelope proves the
    // wildcard route matched and the handler produced the taxonomy error.
    assert_eq!(resp.status(), 404);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "LM-2001");
    assert_eq!(body["error"]["type"], "invalid_request");
    assert_eq!(body["error"]["message"], "model 'no-such/model' not found");
}

#[tokio::test]
async fn retrieve_unknown_model_is_404_lm2001() {
    let base = common::spawn_with(registry(), LIMIT).await;

    let resp = reqwest::get(format!("{base}/v1/models/no-such-model"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "LM-2001");
    assert_eq!(body["error"]["type"], "invalid_request");
    assert_eq!(body["error"]["message"], "model 'no-such-model' not found");
}

#[tokio::test]
async fn empty_config_lists_no_models() {
    let base = common::spawn_with(common::empty_registry(), LIMIT).await;

    let resp = reqwest::get(format!("{base}/v1/models")).await.unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["object"], "list");
    assert!(body["data"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn release_date_is_exposed_for_sorting_and_omitted_when_unset() {
    let base = common::spawn_with(registry(), LIMIT).await;

    let body: Value = reqwest::get(format!("{base}/v1/models"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let data = body["data"].as_array().unwrap();

    // A dated model carries the ISO date plus the OpenAI-compatible integer
    // `created` (midnight UTC of that date), so either can be sorted on.
    let gpt = data.iter().find(|m| m["id"] == "gpt").unwrap();
    assert_eq!(gpt["release_date"], "2024-05-13");
    assert_eq!(gpt["created"], 1_715_558_400_u64);

    // An undated model omits both rather than reporting a fake epoch 0.
    let multi = data.iter().find(|m| m["id"] == "multi").unwrap();
    assert!(multi.get("release_date").is_none());
    assert!(multi.get("created").is_none());

    // Retrieve serves the same fields.
    let retrieved: Value = reqwest::get(format!("{base}/v1/models/gpt"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(retrieved, *gpt);
}

#[tokio::test]
async fn virtual_models_are_listed_and_retrievable_unless_hidden() {
    let cfg = lumen_server::config::Config::load_text(
        r#"
        [[providers]]
        name = "openai"
        kind = "openai"
        [[providers.models]]
        id = "gpt-4o"
        capabilities = ["chat"]
        modalities = ["text", "image"]

        [[virtual_models]]
        id = "acme/chat"
        capability = "chat"
        strategy = "single"
        description = "Acme chat"
        targets = [{ model = "acme/hidden" }]

        [[virtual_models]]
        id = "acme/hidden"
        capability = "chat"
        strategy = "single"
        listed = false
        targets = [{ model = "gpt-4o" }]
        "#,
        "test",
    )
    .unwrap();
    let registry = Arc::new(
        Registry::build(
            cfg.provider_specs(),
            http::build_client(),
            std::time::Duration::from_secs(300),
        )
        .unwrap(),
    );
    let state = common::base_state(registry).with_resilience(Arc::new(
        lumen_server::resilience::ResilienceRuntime::from_config(&cfg, None),
    ));
    let base = common::spawn_state(state, LIMIT).await;

    let list: Value = reqwest::get(format!("{base}/v1/models"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ids: Vec<&str> = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["gpt-4o", "acme/chat"]);
    let entry = &list["data"][1];
    assert_eq!(entry["virtual"], true);
    assert_eq!(entry["capabilities"], serde_json::json!(["chat"]));
    assert_eq!(entry["description"], "Acme chat");
    assert_eq!(entry["modalities"], serde_json::json!(["text", "image"]));
    assert_eq!(entry["owned_by"], "lumen");
    assert_eq!(list["data"][0]["virtual"], false);

    let one = reqwest::get(format!("{base}/v1/models/acme/chat"))
        .await
        .unwrap();
    assert_eq!(one.status(), 200);
    let hidden = reqwest::get(format!("{base}/v1/models/acme/hidden"))
        .await
        .unwrap();
    assert_eq!(hidden.status(), 404);
}
