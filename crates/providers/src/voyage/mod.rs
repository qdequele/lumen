//! Voyage AI provider — embeddings and reranking.
//!
//! Voyage's embeddings endpoint is OpenAI-compatible (near-passthrough). Its
//! rerank endpoint differs from Cohere's in two field names: the request uses
//! `top_k` (not `top_n`) and the response nests results under `data` (not
//! `results`). Voyage bills reranking in tokens, so `usage.search_units` is
//! reported as `0`.

use async_trait::async_trait;
use ferrogate_core::{
    EmbedRequest, EmbedResponse, EmbeddingProvider, ProviderError, RerankProvider, RerankRequest,
    RerankResponse, RerankResult, RerankUsage,
};
use serde::{Deserialize, Serialize};
use std::fmt;
use tokio_util::sync::CancellationToken;

use crate::http::post_json;

/// Default Voyage API base (includes the `/v1` prefix).
const DEFAULT_BASE_URL: &str = "https://api.voyageai.com/v1";

/// Conservative batch ceiling for Voyage embeddings.
const MAX_BATCH_SIZE: usize = 128;

/// A Voyage provider serving embeddings and reranking.
pub struct VoyageProvider {
    client: reqwest::Client,
    provider_name: String,
    base_url: String,
    /// Bearer token. Redacted from `Debug`; never logged.
    api_key: Option<String>,
}

impl VoyageProvider {
    /// Construct a provider. `base_url` defaults to the public Voyage API.
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
impl fmt::Debug for VoyageProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VoyageProvider")
            .field("provider_name", &self.provider_name)
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl EmbeddingProvider for VoyageProvider {
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
            .map_err(|e| ProviderError::Translation(format!("voyage embeddings response: {e}")))
    }

    fn max_batch_size(&self) -> usize {
        MAX_BATCH_SIZE
    }
}

// ---- Reranking -----------------------------------------------------------

#[derive(Serialize)]
struct VoyageRerankRequest<'a> {
    model: &'a str,
    query: &'a str,
    documents: Vec<&'a str>,
    /// Voyage's name for `top_n`.
    #[serde(skip_serializing_if = "Option::is_none")]
    top_k: Option<u32>,
}

#[derive(Deserialize)]
struct VoyageRerankResponse {
    /// Voyage nests results under `data`, not `results`.
    #[serde(default)]
    data: Vec<VoyageRerankResult>,
}

#[derive(Deserialize)]
struct VoyageRerankResult {
    index: u32,
    relevance_score: f32,
}

#[async_trait]
impl RerankProvider for VoyageProvider {
    async fn rerank(
        &self,
        req: RerankRequest,
        cancel: CancellationToken,
    ) -> Result<RerankResponse, ProviderError> {
        let url = format!("{}/rerank", self.base_url);
        let documents: Vec<&str> = req
            .documents
            .iter()
            .map(ferrogate_core::RerankDocument::text)
            .collect();
        let body = VoyageRerankRequest {
            model: &req.model,
            query: &req.query,
            documents,
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

        let parsed: VoyageRerankResponse = serde_json::from_slice(&bytes)
            .map_err(|e| ProviderError::Translation(format!("voyage rerank response: {e}")))?;

        Ok(RerankResponse {
            results: parsed
                .data
                .into_iter()
                .map(|r| RerankResult {
                    index: r.index,
                    relevance_score: r.relevance_score,
                    document: None,
                })
                .collect(),
            // Voyage bills in tokens, not search units; the field does not apply.
            usage: RerankUsage { search_units: 0 },
        })
    }
}
