//! End-to-end HTTP tests for `GET /v1/models`: the list reflects only the
//! operator's configuration (no upstream introspection) and reports each
//! model's capabilities, including multi-capability models.

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
            strict: false,
            connect_timeout_ms: None,
            models: vec![ModelSpec {
                id: "multi".to_owned(),
                upstream_id: "embed-v4.0".to_owned(),
                // A single Cohere model configured for BOTH embed and rerank.
                capabilities: vec![Capability::Embed, Capability::Rerank],
                modalities: vec!["text".to_owned()],
            }],
        },
        ProviderSpec {
            name: "openai".to_owned(),
            kind: ProviderKind::Openai,
            api_key: Some("sk-test-xxx".to_owned()),
            base_url: None,
            strict: false,
            connect_timeout_ms: None,
            models: vec![ModelSpec {
                id: "gpt".to_owned(),
                upstream_id: "gpt-4o".to_owned(),
                capabilities: vec![Capability::Chat],
                modalities: vec!["text".to_owned()],
            }],
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
    assert_eq!(data.len(), 2);

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
async fn empty_config_lists_no_models() {
    let base = common::spawn_with(common::empty_registry(), LIMIT).await;

    let resp = reqwest::get(format!("{base}/v1/models")).await.unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["object"], "list");
    assert!(body["data"].as_array().unwrap().is_empty());
}
