//! Mixedbread provider - reranking (`mxbai-rerank-*`).
//!
//! Mixedbread's rerank endpoint (`POST /rerank`) is close to Cohere's but renames
//! two fields: the request carries `input` (not `documents`) and `top_k` (not
//! `top_n`); the response nests scored items under `data`, each `{ index, score }`
//! (accepted as `results` / `relevance_score` too, for robustness against the
//! Cohere-compatible variant some deployments expose). Auth is a bearer token.
//!
//! Mixedbread bills reranking in tokens rather than search units, so
//! `usage.search_units` is left at `0` and the gateway derives an ADR-003 token
//! estimate marked `estimated`, exactly as for Jina and Voyage.

use async_trait::async_trait;
use lumen_core::{
    ProviderError, RerankProvider, RerankRequest, RerankResponse, RerankResult, RerankUsage,
};
use serde::{Deserialize, Serialize};
use std::fmt;
use tokio_util::sync::CancellationToken;

use crate::http::post_json;

/// Default Mixedbread API base (includes the `/v1` prefix).
const DEFAULT_BASE_URL: &str = "https://api.mixedbread.com/v1";

/// A Mixedbread provider serving reranking.
pub struct MixedbreadProvider {
    client: reqwest::Client,
    provider_name: String,
    base_url: String,
    /// Bearer token. Redacted from `Debug`; never logged.
    api_key: Option<String>,
}

impl MixedbreadProvider {
    /// Construct a provider. `base_url` defaults to the public Mixedbread API.
    #[must_use]
    pub fn new(
        client: reqwest::Client,
        provider_name: impl Into<String>,
        base_url: Option<String>,
        api_key: Option<String>,
    ) -> Self {
        let base_url = base_url
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_owned())
            .trim_end_matches('/')
            .to_owned();
        Self {
            client,
            provider_name: provider_name.into(),
            base_url,
            api_key,
        }
    }
}

/// Redacted so the API key can never reach a log line via `{:?}`.
impl fmt::Debug for MixedbreadProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MixedbreadProvider")
            .field("provider_name", &self.provider_name)
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .finish_non_exhaustive()
    }
}

#[derive(Serialize)]
struct MixedbreadRerankRequest<'a> {
    model: &'a str,
    query: &'a str,
    /// Mixedbread's name for `documents`.
    input: Vec<&'a str>,
    /// Mixedbread's name for `top_n`.
    #[serde(skip_serializing_if = "Option::is_none")]
    top_k: Option<u32>,
}

#[derive(Deserialize)]
struct MixedbreadRerankResponse {
    /// Mixedbread nests results under `data`; a Cohere-compatible deployment
    /// may use `results` instead.
    #[serde(default, alias = "results")]
    data: Vec<MixedbreadRerankResult>,
}

#[derive(Deserialize)]
struct MixedbreadRerankResult {
    index: u32,
    /// Mixedbread names the score `score`; the Cohere-compatible variant uses
    /// `relevance_score`.
    #[serde(alias = "relevance_score")]
    score: f32,
}

#[async_trait]
impl RerankProvider for MixedbreadProvider {
    async fn rerank(
        &self,
        req: RerankRequest,
        cancel: CancellationToken,
    ) -> Result<RerankResponse, ProviderError> {
        let url = format!("{}/rerank", self.base_url);
        let input: Vec<&str> = req
            .documents
            .iter()
            .map(lumen_core::RerankDocument::text)
            .collect();
        let body = MixedbreadRerankRequest {
            model: &req.model,
            query: &req.query,
            input,
            top_k: req.top_n,
        };

        let bytes = post_json(
            &self.client,
            &url,
            &body,
            self.api_key.as_deref(),
            &self.provider_name,
            &cancel,
        )
        .await?;

        let parsed: MixedbreadRerankResponse = serde_json::from_slice(&bytes)
            .map_err(|e| ProviderError::Translation(format!("mixedbread rerank response: {e}")))?;

        Ok(RerankResponse {
            results: parsed
                .data
                .into_iter()
                .map(|r| RerankResult {
                    index: r.index,
                    relevance_score: r.score,
                    document: None,
                })
                .collect(),
            // Mixedbread bills in tokens, not search units; the gateway derives
            // the ADR-003 estimate (see `server::rerank`).
            usage: RerankUsage::default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_leaks_the_api_key() {
        let provider = MixedbreadProvider::new(
            reqwest::Client::new(),
            "mxbai",
            None,
            Some("sk-test-super-secret".to_owned()),
        );
        let debug = format!("{provider:?}");
        assert!(!debug.contains("sk-test-super-secret"));
        assert!(debug.contains("<redacted>"));
    }
}
