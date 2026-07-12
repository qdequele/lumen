//! Cohere provider (API v2) — embeddings and reranking.
//!
//! Cohere's wire schema differs from the internal (OpenAI/Cohere-inspired)
//! types in both directions, so this module translates:
//!
//! * embed: `POST /v2/embed` takes `{ model, texts, input_type, embedding_types }`
//!   and returns `{ embeddings: { float: [[..]] }, meta: { billed_units } }`;
//! * rerank: `POST /v2/rerank` takes `{ model, query, documents, top_n }` and
//!   returns `{ results: [{ index, relevance_score }], meta: { billed_units } }`.
//!
//! The gateway (`crate::rerank`) owns ordering, `top_n` clamping and document
//! echoing, so the rerank translation only carries indices, scores and usage.

use async_trait::async_trait;
use ferrogate_core::{
    EmbedData, EmbedRequest, EmbedResponse, EmbedUsage, EmbeddingProvider, ProviderError,
    RerankProvider, RerankRequest, RerankResponse, RerankResult, RerankUsage,
};
use serde::{Deserialize, Serialize};
use std::fmt;
use tokio_util::sync::CancellationToken;

use crate::http::post_json;

/// Default Cohere API base (no version suffix; paths add `/v2/...`).
const DEFAULT_BASE_URL: &str = "https://api.cohere.com";

/// Cohere's documented maximum number of texts per embed request.
const MAX_BATCH_SIZE: usize = 96;

/// A Cohere provider serving embeddings and reranking.
pub struct CohereProvider {
    client: reqwest::Client,
    provider_name: String,
    base_url: String,
    /// Bearer token. Redacted from `Debug`; never logged.
    api_key: Option<String>,
}

impl CohereProvider {
    /// Construct a provider. `base_url` defaults to the public Cohere API.
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
impl fmt::Debug for CohereProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CohereProvider")
            .field("provider_name", &self.provider_name)
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .finish_non_exhaustive()
    }
}

// ---- Embeddings ----------------------------------------------------------

#[derive(Serialize)]
struct CohereEmbedRequest<'a> {
    model: &'a str,
    texts: Vec<&'a str>,
    /// Required by v2. The gateway does not know query-vs-document intent, so it
    /// defaults to `search_document` (the indexing case).
    input_type: &'static str,
    embedding_types: [&'static str; 1],
}

#[derive(Deserialize)]
struct CohereEmbedResponse {
    embeddings: CohereEmbeddings,
    #[serde(default)]
    meta: CohereMeta,
}

#[derive(Deserialize)]
struct CohereEmbeddings {
    #[serde(default)]
    float: Vec<Vec<f32>>,
}

#[derive(Default, Deserialize)]
struct CohereMeta {
    #[serde(default)]
    billed_units: CohereBilledUnits,
}

#[derive(Default, Deserialize)]
struct CohereBilledUnits {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    search_units: u32,
}

#[async_trait]
impl EmbeddingProvider for CohereProvider {
    async fn embed(
        &self,
        req: EmbedRequest,
        cancel: CancellationToken,
    ) -> Result<EmbedResponse, ProviderError> {
        let url = format!("{}/v2/embed", self.base_url);
        let body = CohereEmbedRequest {
            model: &req.model,
            texts: req.input.iter().collect(),
            input_type: "search_document",
            embedding_types: ["float"],
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

        let parsed: CohereEmbedResponse = serde_json::from_slice(&bytes)
            .map_err(|e| ProviderError::Translation(format!("cohere embed response: {e}")))?;

        let data = parsed
            .embeddings
            .float
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
            usage: EmbedUsage {
                prompt_tokens: parsed.meta.billed_units.input_tokens,
                total_tokens: parsed.meta.billed_units.input_tokens,
            },
        })
    }

    fn max_batch_size(&self) -> usize {
        MAX_BATCH_SIZE
    }
}

// ---- Reranking -----------------------------------------------------------

#[derive(Serialize)]
struct CohereRerankRequest<'a> {
    model: &'a str,
    query: &'a str,
    documents: Vec<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_n: Option<u32>,
}

#[derive(Deserialize)]
struct CohereRerankResponse {
    #[serde(default)]
    results: Vec<CohereRerankResult>,
    #[serde(default)]
    meta: CohereMeta,
}

#[derive(Deserialize)]
struct CohereRerankResult {
    index: u32,
    relevance_score: f32,
}

#[async_trait]
impl RerankProvider for CohereProvider {
    async fn rerank(
        &self,
        req: RerankRequest,
        cancel: CancellationToken,
    ) -> Result<RerankResponse, ProviderError> {
        let url = format!("{}/v2/rerank", self.base_url);
        let documents: Vec<&str> = req
            .documents
            .iter()
            .map(ferrogate_core::RerankDocument::text)
            .collect();
        let body = CohereRerankRequest {
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

        let parsed: CohereRerankResponse = serde_json::from_slice(&bytes)
            .map_err(|e| ProviderError::Translation(format!("cohere rerank response: {e}")))?;

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
            usage: RerankUsage {
                search_units: parsed.meta.billed_units.search_units,
            },
        })
    }
}
