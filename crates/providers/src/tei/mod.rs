//! TEI (Hugging Face Text Embeddings Inference) provider — self-hosted.
//!
//! TEI serves one model per process and has no OpenAI-compatible envelope:
//!
//! * embed: `POST /embed` takes `{ inputs: [..] }` and returns a bare array of
//!   float arrays (`[[..], ..]`) with no usage;
//! * rerank: `POST /rerank` takes `{ query, texts }` and returns a bare array of
//!   `{ index, score }` with no usage.
//!
//! It is keyless by default; an optional bearer token supports deployments
//! placed behind an authenticating proxy. A `base_url` is always required.

use async_trait::async_trait;
use ferrogate_core::{
    EmbedData, EmbedRequest, EmbedResponse, EmbedUsage, EmbeddingProvider, ProviderError,
    RerankProvider, RerankRequest, RerankResponse, RerankResult, RerankUsage,
};
use serde::{Deserialize, Serialize};
use std::fmt;
use tokio_util::sync::CancellationToken;

use crate::http::post_json;

/// Conservative default matching TEI's own `--max-client-batch-size` default.
const MAX_BATCH_SIZE: usize = 32;

/// A TEI provider serving embeddings and reranking from a self-hosted endpoint.
pub struct TeiProvider {
    client: reqwest::Client,
    provider_name: String,
    base_url: String,
    /// Optional bearer token (redacted from `Debug`; usually absent).
    api_key: Option<String>,
}

impl TeiProvider {
    /// Construct a provider pointed at `base_url` (trailing slash trimmed).
    #[must_use]
    pub fn new(
        client: reqwest::Client,
        provider_name: impl Into<String>,
        base_url: impl Into<String>,
        api_key: Option<String>,
    ) -> Self {
        Self {
            client,
            provider_name: provider_name.into(),
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            api_key,
        }
    }
}

/// Redacted so any bearer token can never reach a log line via `{:?}`.
impl fmt::Debug for TeiProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TeiProvider")
            .field("provider_name", &self.provider_name)
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .finish_non_exhaustive()
    }
}

// ---- Embeddings ----------------------------------------------------------

#[derive(Serialize)]
struct TeiEmbedRequest<'a> {
    inputs: Vec<&'a str>,
}

#[async_trait]
impl EmbeddingProvider for TeiProvider {
    async fn embed(
        &self,
        req: EmbedRequest,
        cancel: CancellationToken,
    ) -> Result<EmbedResponse, ProviderError> {
        let url = format!("{}/embed", self.base_url);
        let body = TeiEmbedRequest {
            inputs: req.input.iter().collect(),
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

        // TEI returns a bare array of float arrays.
        let vectors: Vec<Vec<f32>> = serde_json::from_slice(&bytes)
            .map_err(|e| ProviderError::Translation(format!("tei embed response: {e}")))?;

        let data = vectors
            .into_iter()
            .enumerate()
            .map(|(index, embedding)| EmbedData {
                object: "embedding".to_owned(),
                index: u32::try_from(index).unwrap_or(u32::MAX),
                embedding,
            })
            .collect();

        Ok(EmbedResponse {
            object: "list".to_owned(),
            data,
            model: req.model,
            // TEI reports no token usage.
            usage: EmbedUsage::default(),
        })
    }

    fn max_batch_size(&self) -> usize {
        MAX_BATCH_SIZE
    }
}

// ---- Reranking -----------------------------------------------------------

#[derive(Serialize)]
struct TeiRerankRequest<'a> {
    query: &'a str,
    texts: Vec<&'a str>,
}

#[derive(Deserialize)]
struct TeiRerankResult {
    index: u32,
    score: f32,
}

#[async_trait]
impl RerankProvider for TeiProvider {
    async fn rerank(
        &self,
        req: RerankRequest,
        cancel: CancellationToken,
    ) -> Result<RerankResponse, ProviderError> {
        let url = format!("{}/rerank", self.base_url);
        let texts: Vec<&str> = req
            .documents
            .iter()
            .map(ferrogate_core::RerankDocument::text)
            .collect();
        // TEI has no `top_n`; the gateway truncates after sorting (see
        // `crate::rerank`).
        let body = TeiRerankRequest {
            query: &req.query,
            texts,
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

        // TEI returns a bare array of { index, score }.
        let results: Vec<TeiRerankResult> = serde_json::from_slice(&bytes)
            .map_err(|e| ProviderError::Translation(format!("tei rerank response: {e}")))?;

        Ok(RerankResponse {
            results: results
                .into_iter()
                .map(|r| RerankResult {
                    index: r.index,
                    relevance_score: r.score,
                    document: None,
                })
                .collect(),
            // TEI reports no usage.
            usage: RerankUsage::default(),
        })
    }
}
