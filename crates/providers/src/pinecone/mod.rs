//! Pinecone provider - reranking (hosted inference).
//!
//! Pinecone's rerank endpoint (`POST /rerank`) differs from the others in three
//! ways:
//!
//! * auth is an `Api-Key` header (not a bearer token), alongside a pinned
//!   `X-Pinecone-API-Version` header the inference API requires;
//! * documents are sent as objects (`{ "text": ... }`), not bare strings;
//! * the response nests scored items under `data` (`{ index, score }`) and,
//!   unlike the token-billed rerankers, reports real usage as
//!   `usage.rerank_units`, which maps directly onto our
//!   [`RerankUsage::search_units`] (so the gateway uses it verbatim rather than
//!   deriving an estimate).
//!
//! Only the default `text` rank field is used; Cohere-style `rank_fields`
//! selection over arbitrary document objects is intentionally out of scope for
//! v1 (see `docs/backlog.md`).

use async_trait::async_trait;
use lumen_core::{
    ProviderError, RerankProvider, RerankRequest, RerankResponse, RerankResult, RerankUsage,
};
use serde::{Deserialize, Serialize};
use std::fmt;
use tokio_util::sync::CancellationToken;

use crate::http::post_json_with_headers;

/// Default Pinecone API base (the inference host; no version in the path).
const DEFAULT_BASE_URL: &str = "https://api.pinecone.io";

/// The inference API version header Pinecone requires on `/rerank`. Pinned to a
/// known-good dated release; bump deliberately when adopting a newer schema.
const API_VERSION: &str = "2025-01";

/// A Pinecone provider serving reranking.
pub struct PineconeProvider {
    client: reqwest::Client,
    provider_name: String,
    base_url: String,
    /// The Pinecone API key, sent as the `Api-Key` header. Redacted from
    /// `Debug`; never logged.
    api_key: Option<String>,
}

impl PineconeProvider {
    /// Construct a provider. `base_url` defaults to the public Pinecone API.
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
impl fmt::Debug for PineconeProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PineconeProvider")
            .field("provider_name", &self.provider_name)
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .finish_non_exhaustive()
    }
}

#[derive(Serialize)]
struct PineconeDocument<'a> {
    text: &'a str,
}

#[derive(Serialize)]
struct PineconeRerankRequest<'a> {
    model: &'a str,
    query: &'a str,
    documents: Vec<PineconeDocument<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_n: Option<u32>,
}

#[derive(Deserialize)]
struct PineconeRerankResponse {
    #[serde(default)]
    data: Vec<PineconeRerankResult>,
    #[serde(default)]
    usage: PineconeUsage,
}

#[derive(Deserialize)]
struct PineconeRerankResult {
    index: u32,
    score: f32,
}

#[derive(Default, Deserialize)]
struct PineconeUsage {
    /// Pinecone's billing unit; maps directly onto our `search_units`.
    #[serde(default)]
    rerank_units: u32,
}

#[async_trait]
impl RerankProvider for PineconeProvider {
    async fn rerank(
        &self,
        req: RerankRequest,
        cancel: CancellationToken,
    ) -> Result<RerankResponse, ProviderError> {
        let url = format!("{}/rerank", self.base_url);
        let documents: Vec<PineconeDocument<'_>> = req
            .documents
            .iter()
            .map(|d| PineconeDocument { text: d.text() })
            .collect();
        let body = PineconeRerankRequest {
            model: &req.model,
            query: &req.query,
            documents,
            top_n: req.top_n,
        };

        // Pinecone authenticates with an `Api-Key` header (not a bearer token)
        // and requires a pinned API-version header. A `None` key is sent as an
        // empty header so the upstream returns its own 401 rather than the
        // gateway masking the misconfiguration.
        let headers = [
            ("Api-Key", self.api_key.as_deref().unwrap_or("")),
            ("X-Pinecone-API-Version", API_VERSION),
        ];
        let bytes = post_json_with_headers(
            &self.client,
            &url,
            &body,
            &headers,
            &self.provider_name,
            &cancel,
        )
        .await?;

        let parsed: PineconeRerankResponse = serde_json::from_slice(&bytes)
            .map_err(|e| ProviderError::Translation(format!("pinecone rerank response: {e}")))?;

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
            // Pinecone reports real usage; carry it through verbatim (the
            // gateway estimates only when this is 0).
            usage: RerankUsage {
                search_units: parsed.usage.rerank_units,
                estimated: None,
                // Pinecone does not report a token count; the gateway derives
                // one for uniform observability (ADR 003), see
                // `lumen_server::rerank`.
                ..Default::default()
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_leaks_the_api_key() {
        let provider = PineconeProvider::new(
            reqwest::Client::new(),
            "pinecone",
            None,
            Some("pcsk-super-secret".to_owned()),
        );
        let debug = format!("{provider:?}");
        assert!(!debug.contains("pcsk-super-secret"));
        assert!(debug.contains("<redacted>"));
    }
}
