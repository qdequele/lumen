//! NVIDIA NIM provider - reranking (`POST {base}/v1/ranking`).
//!
//! NVIDIA's NIM ranking microservice has its own request/response shape:
//!
//! * request: `{ model, query: { text }, passages: [{ text }, ...] }` - note the
//!   query and each passage are objects with a `text` field, and there is no
//!   `top_n` (NIM returns a ranking over every passage; the gateway truncates to
//!   `top_n` afterwards, exactly as for TEI);
//! * response: `{ rankings: [{ index, logit }, ...] }`.
//!
//! **Score semantics**: NIM reports a raw `logit`, not a normalised probability.
//! It is passed through unchanged as `relevance_score`, so scores are unbounded
//! (can be negative) and only meaningful *relative to each other* within one
//! response; higher is more relevant. The gateway sorts by descending score and
//! does not transform it (no sigmoid is applied - the issue does not ask for
//! one, and squashing would discard information callers may want).
//!
//! `base_url` is required (a NIM is self-hosted or a specific NVIDIA-hosted
//! endpoint). Auth is an optional bearer token (self-hosted NIMs are typically
//! keyless; the hosted API expects a key). NIM reports no token usage, so
//! `usage.search_units` stays `0` and the gateway derives an ADR-003 estimate.

use async_trait::async_trait;
use lumen_core::{
    ProviderError, RerankProvider, RerankRequest, RerankResponse, RerankResult, RerankUsage,
};
use serde::{Deserialize, Serialize};
use std::fmt;
use tokio_util::sync::CancellationToken;

use crate::http::post_json;

/// A NVIDIA NIM provider serving reranking via `POST {base}/v1/ranking`.
pub struct NvidiaProvider {
    client: reqwest::Client,
    provider_name: String,
    base_url: String,
    /// Optional bearer token. Redacted from `Debug`; never logged.
    api_key: Option<String>,
}

impl NvidiaProvider {
    /// Construct a provider. `base_url` is required (the NIM root, e.g.
    /// `http://localhost:8000` or the hosted ranking endpoint root).
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

/// Redacted so the API key can never reach a log line via `{:?}`.
impl fmt::Debug for NvidiaProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NvidiaProvider")
            .field("provider_name", &self.provider_name)
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .finish_non_exhaustive()
    }
}

#[derive(Serialize)]
struct NvidiaText<'a> {
    text: &'a str,
}

#[derive(Serialize)]
struct NvidiaRerankRequest<'a> {
    model: &'a str,
    query: NvidiaText<'a>,
    passages: Vec<NvidiaText<'a>>,
}

#[derive(Deserialize)]
struct NvidiaRerankResponse {
    #[serde(default)]
    rankings: Vec<NvidiaRanking>,
}

#[derive(Deserialize)]
struct NvidiaRanking {
    index: u32,
    /// A raw logit (unbounded, may be negative); passed through as the score.
    logit: f32,
}

#[async_trait]
impl RerankProvider for NvidiaProvider {
    async fn rerank(
        &self,
        req: RerankRequest,
        cancel: CancellationToken,
    ) -> Result<RerankResponse, ProviderError> {
        let url = format!("{}/v1/ranking", self.base_url);
        let passages: Vec<NvidiaText<'_>> = req
            .documents
            .iter()
            .map(|d| NvidiaText { text: d.text() })
            .collect();
        let body = NvidiaRerankRequest {
            model: &req.model,
            query: NvidiaText { text: &req.query },
            passages,
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

        let parsed: NvidiaRerankResponse = serde_json::from_slice(&bytes)
            .map_err(|e| ProviderError::Translation(format!("nvidia rerank response: {e}")))?;

        Ok(RerankResponse {
            results: parsed
                .rankings
                .into_iter()
                .map(|r| RerankResult {
                    index: r.index,
                    // Raw logit passed through unchanged (see module docs).
                    relevance_score: r.logit,
                    document: None,
                })
                .collect(),
            // NIM reports no usage; the gateway derives the ADR-003 estimate.
            usage: RerankUsage::default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_leaks_the_api_key() {
        let provider = NvidiaProvider::new(
            reqwest::Client::new(),
            "nvidia",
            "http://localhost:8000",
            Some("nvapi-super-secret".to_owned()),
        );
        let debug = format!("{provider:?}");
        assert!(!debug.contains("nvapi-super-secret"));
        assert!(debug.contains("<redacted>"));
    }
}
