//! Jina AI provider - embeddings and reranking.
//!
//! Jina's embeddings endpoint is OpenAI-compatible, so the embed path is a
//! near-passthrough (like [`crate::openai`]). Reranking (`POST /rerank`) is
//! Cohere-shaped but bills in tokens rather than search units, so
//! `usage.search_units` is reported as `0` (the value does not apply).

use async_trait::async_trait;
use lumen_core::{
    EmbedRequest, EmbedResponse, EmbeddingProvider, ProviderError, RerankProvider, RerankRequest,
    RerankResponse, RerankResult, RerankUsage,
};
use serde::{Deserialize, Serialize};
use std::fmt;
use tokio_util::sync::CancellationToken;

use crate::http::post_json;

/// Default Jina API base (includes the `/v1` prefix).
const DEFAULT_BASE_URL: &str = "https://api.jina.ai/v1";

/// Conservative batch ceiling for Jina embeddings.
const MAX_BATCH_SIZE: usize = 2048;

/// A Jina provider serving embeddings and reranking.
pub struct JinaProvider {
    client: reqwest::Client,
    provider_name: String,
    base_url: String,
    /// Bearer token. Redacted from `Debug`; never logged.
    api_key: Option<String>,
}

impl JinaProvider {
    /// Construct a provider. `base_url` defaults to the public Jina API.
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
impl fmt::Debug for JinaProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JinaProvider")
            .field("provider_name", &self.provider_name)
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl EmbeddingProvider for JinaProvider {
    async fn embed(
        &self,
        req: EmbedRequest,
        cancel: CancellationToken,
    ) -> Result<EmbedResponse, ProviderError> {
        // OpenAI-compatible schema: near-passthrough in both directions.
        let url = format!("{}/embeddings", self.base_url);
        let bytes = post_json(
            &self.client,
            &url,
            &req,
            self.api_key.as_deref(),
            &self.provider_name,
            &cancel,
        )
        .await?;
        serde_json::from_slice::<EmbedResponse>(&bytes)
            .map_err(|e| ProviderError::Translation(format!("jina embeddings response: {e}")))
    }

    fn max_batch_size(&self) -> usize {
        MAX_BATCH_SIZE
    }
}

// ---- Reranking -----------------------------------------------------------

#[derive(Serialize)]
struct JinaRerankRequest<'a> {
    model: &'a str,
    query: &'a str,
    documents: Vec<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_n: Option<u32>,
}

#[derive(Deserialize)]
struct JinaRerankResponse {
    #[serde(default)]
    results: Vec<JinaRerankResult>,
}

#[derive(Deserialize)]
struct JinaRerankResult {
    index: u32,
    relevance_score: f32,
}

#[async_trait]
impl RerankProvider for JinaProvider {
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
        let body = JinaRerankRequest {
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

        let parsed: JinaRerankResponse = serde_json::from_slice(&bytes)
            .map_err(|e| ProviderError::Translation(format!("jina rerank response: {e}")))?;

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
            // Jina bills in tokens, not search units; the field does not apply.
            usage: RerankUsage {
                search_units: 0,
                estimated: None,
            },
        })
    }
}
