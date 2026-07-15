//! Together AI provider - native reranking (LlamaRank).
//!
//! Together serves chat and embeddings through an OpenAI-compatible endpoint
//! (see [`crate::openai`], wired for the `together` kind in [`crate::registry`]),
//! but its rerank models (`Salesforce/Llama-Rank-*`) live on a native
//! `POST /rerank` endpoint whose schema is Cohere-compatible:
//!
//! * request: `{ model, query, documents: [...], top_n? }`
//! * response: `{ results: [{ index, relevance_score }, ...] }`
//!
//! Auth is a bearer token, the same key used for chat/embed. One `[[providers]]`
//! entry with `kind = "together"` therefore serves all three capabilities
//! against the same `base_url`, mirroring how the `cloudflare` kind adds a
//! native rerank provider alongside its OpenAI-compatible chat/embed wiring.
//!
//! Together bills reranking in tokens, so `usage.search_units` is left at `0`
//! and the gateway derives an ADR-003 token estimate marked `estimated`.

use async_trait::async_trait;
use lumen_core::{
    ProviderError, RerankProvider, RerankRequest, RerankResponse, RerankResult, RerankUsage,
};
use serde::{Deserialize, Serialize};
use std::fmt;
use tokio_util::sync::CancellationToken;

use crate::http::post_json;

/// Default Together API base (includes the `/v1` prefix). Matches the
/// `together` kind's OpenAI-compatible default so one `base_url` serves all
/// three capabilities.
const DEFAULT_BASE_URL: &str = "https://api.together.xyz/v1";

/// A Together provider serving reranking via the native `POST /rerank`
/// endpoint. Chat and embeddings for the `together` kind are served separately,
/// by [`crate::openai::OpenAiProvider`] against the same `base_url` (see
/// `crate::registry`).
pub struct TogetherRerankProvider {
    client: reqwest::Client,
    provider_name: String,
    base_url: String,
    /// Bearer token. Redacted from `Debug`; never logged.
    api_key: Option<String>,
}

impl TogetherRerankProvider {
    /// Construct a provider. `base_url` defaults to the public Together API.
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
impl fmt::Debug for TogetherRerankProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TogetherRerankProvider")
            .field("provider_name", &self.provider_name)
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .finish_non_exhaustive()
    }
}

#[derive(Serialize)]
struct TogetherRerankRequest<'a> {
    model: &'a str,
    query: &'a str,
    documents: Vec<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_n: Option<u32>,
}

#[derive(Deserialize)]
struct TogetherRerankResponse {
    #[serde(default)]
    results: Vec<TogetherRerankResult>,
}

#[derive(Deserialize)]
struct TogetherRerankResult {
    index: u32,
    relevance_score: f32,
}

#[async_trait]
impl RerankProvider for TogetherRerankProvider {
    async fn rerank(
        &self,
        req: RerankRequest,
        cancel: CancellationToken,
    ) -> Result<RerankResponse, ProviderError> {
        let url = format!("{}/rerank", self.base_url);
        let documents: Vec<&str> = req
            .documents
            .iter()
            .map(lumen_core::RerankDocument::text)
            .collect();
        let body = TogetherRerankRequest {
            model: &req.model,
            query: &req.query,
            documents,
            top_n: req.top_n,
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

        let parsed: TogetherRerankResponse = serde_json::from_slice(&bytes)
            .map_err(|e| ProviderError::Translation(format!("together rerank response: {e}")))?;

        Ok(RerankResponse {
            results: parsed
                .results
                .into_iter()
                .map(|r| RerankResult {
                    index: r.index,
                    relevance_score: r.relevance_score,
                    document: None,
                })
                .collect(),
            // Together bills in tokens, not search units; the gateway derives
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
        let provider = TogetherRerankProvider::new(
            reqwest::Client::new(),
            "together",
            None,
            Some("sk-together-super-secret".to_owned()),
        );
        let debug = format!("{provider:?}");
        assert!(!debug.contains("sk-together-super-secret"));
        assert!(debug.contains("<redacted>"));
    }
}
