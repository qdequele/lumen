//! Cloudflare Workers AI provider - native reranking.
//!
//! Cloudflare Workers AI serves chat and embeddings through an OpenAI-compatible
//! endpoint (see [`crate::openai`], wired for the `cloudflare` kind in
//! [`crate::registry`]), but its BAAI `bge-reranker-*` models are only exposed
//! through the native `POST /ai/run/{model}` endpoint, which has its own
//! request/response shape:
//!
//! * request: `{ query, contexts: [{ text }, ...], top_k? }`
//! * response: the standard Cloudflare API envelope,
//!   `{ result: { response: [{ id, score }, ...] }, success, errors, messages }`
//!
//! `id` is documented by Cloudflare as "index of the context in the request",
//! i.e. it maps directly onto our [`RerankResult::index`]. Cloudflare reports
//! no token/billing usage for this model, so `usage` is left at its default
//! and the gateway (`crate::rerank`, `server::rerank`) derives an ADR-003
//! estimate marked `estimated`, exactly as it does for TEI.
//!
//! The native endpoint lives under the *account root*
//! (`.../accounts/{account_id}/ai/run/{model}`), not under the `/ai/v1` prefix
//! used by the OpenAI-compatible chat/embed path. The gateway config carries a
//! single `base_url` per provider (documented as
//! `.../accounts/{account_id}/ai/v1`); [`CloudflareRerankProvider::run_url`]
//! derives the account root from it by stripping a trailing `/ai/v1` (or bare
//! `/v1`) suffix, so one `[[providers]]` entry serves chat, embed and rerank
//! without a second `base_url`.

use async_trait::async_trait;
use lumen_core::{
    ProviderError, RerankProvider, RerankRequest, RerankResponse, RerankResult, RerankUsage,
};
use serde::{Deserialize, Serialize};
use std::fmt;
use tokio_util::sync::CancellationToken;

use crate::http::post_json;

/// A Cloudflare Workers AI provider serving reranking via the native
/// `POST /ai/run/{model}` endpoint. Chat and embeddings for the `cloudflare`
/// kind are served separately, by [`crate::openai::OpenAiProvider`] against
/// the same `base_url` (see `crate::registry`).
pub struct CloudflareRerankProvider {
    client: reqwest::Client,
    provider_name: String,
    base_url: String,
    /// Bearer token (the Cloudflare API token). Redacted from `Debug`; never
    /// logged.
    api_key: Option<String>,
}

impl CloudflareRerankProvider {
    /// Construct a provider. `base_url` is the same account-scoped URL
    /// configured for chat/embed (see the module docs for how the native
    /// `run` endpoint is derived from it).
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

    /// The native `POST /ai/run/{model}` URL for `model`, derived from the
    /// configured `base_url` by stripping a trailing `/ai/v1` (or bare `/v1`)
    /// suffix so the account root is shared with the OpenAI-compatible path.
    /// A `base_url` that already IS the account root (no such suffix) is used
    /// as-is.
    fn run_url(&self, model: &str) -> String {
        let root = self
            .base_url
            .strip_suffix("/ai/v1")
            .or_else(|| self.base_url.strip_suffix("/v1"))
            .unwrap_or(&self.base_url);
        format!("{root}/ai/run/{model}")
    }
}

/// Redacted so the API token can never reach a log line via `{:?}`.
impl fmt::Debug for CloudflareRerankProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CloudflareRerankProvider")
            .field("provider_name", &self.provider_name)
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .finish_non_exhaustive()
    }
}

#[derive(Serialize)]
struct CloudflareContext<'a> {
    text: &'a str,
}

#[derive(Serialize)]
struct CloudflareRerankRequest<'a> {
    query: &'a str,
    contexts: Vec<CloudflareContext<'a>>,
    /// Cloudflare's name for `top_n`.
    #[serde(skip_serializing_if = "Option::is_none")]
    top_k: Option<u32>,
}

/// Cloudflare's generic API envelope wrapping the model's own output.
#[derive(Deserialize)]
struct CloudflareEnvelope {
    #[serde(default)]
    success: bool,
    #[serde(default)]
    result: Option<CloudflareRerankResult>,
    #[serde(default)]
    errors: Vec<CloudflareApiError>,
}

#[derive(Deserialize, Default)]
struct CloudflareRerankResult {
    #[serde(default)]
    response: Vec<CloudflareRerankItem>,
}

#[derive(Deserialize)]
struct CloudflareRerankItem {
    /// Index of the context in the request (Cloudflare's own field name).
    id: u32,
    score: f32,
}

#[derive(Deserialize)]
struct CloudflareApiError {
    #[serde(default)]
    message: String,
}

#[async_trait]
impl RerankProvider for CloudflareRerankProvider {
    async fn rerank(
        &self,
        req: RerankRequest,
        cancel: CancellationToken,
    ) -> Result<RerankResponse, ProviderError> {
        let url = self.run_url(&req.model);
        let contexts: Vec<CloudflareContext<'_>> = req
            .documents
            .iter()
            .map(|d| CloudflareContext { text: d.text() })
            .collect();
        let body = CloudflareRerankRequest {
            query: &req.query,
            contexts,
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

        let envelope: CloudflareEnvelope = serde_json::from_slice(&bytes)
            .map_err(|e| ProviderError::Translation(format!("cloudflare rerank response: {e}")))?;

        // A 2xx response can still carry a body-level failure per Cloudflare's
        // envelope convention; surface it as a translation error rather than
        // silently returning an empty result set.
        if !envelope.success {
            let detail = envelope
                .errors
                .first()
                .map_or("unknown error", |e| e.message.as_str());
            return Err(ProviderError::Translation(format!(
                "cloudflare rerank request failed: {detail}"
            )));
        }

        let result = envelope.result.ok_or_else(|| {
            ProviderError::Translation("cloudflare rerank response: missing result".to_owned())
        })?;

        Ok(RerankResponse {
            results: result
                .response
                .into_iter()
                .map(|r| RerankResult {
                    index: r.id,
                    relevance_score: r.score,
                    document: None,
                })
                .collect(),
            // Cloudflare reports no usage for this model; ADR 003 estimation
            // happens at the gateway layer (see `server::rerank`), exactly as
            // for TEI.
            usage: RerankUsage::default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_url_strips_the_ai_v1_suffix_used_by_the_openai_compatible_path() {
        let provider = CloudflareRerankProvider::new(
            reqwest::Client::new(),
            "cf",
            "https://api.cloudflare.com/client/v4/accounts/acct123/ai/v1",
            Some("sk-test-xxx".to_owned()),
        );
        assert_eq!(
            provider.run_url("@cf/baai/bge-reranker-base"),
            "https://api.cloudflare.com/client/v4/accounts/acct123/ai/run/@cf/baai/bge-reranker-base"
        );
    }

    #[test]
    fn run_url_accepts_an_account_root_base_url_as_is() {
        let provider = CloudflareRerankProvider::new(
            reqwest::Client::new(),
            "cf",
            "https://api.cloudflare.com/client/v4/accounts/acct123",
            None,
        );
        assert_eq!(
            provider.run_url("@cf/baai/bge-reranker-base"),
            "https://api.cloudflare.com/client/v4/accounts/acct123/ai/run/@cf/baai/bge-reranker-base"
        );
    }

    #[test]
    fn debug_never_leaks_the_api_key() {
        let provider = CloudflareRerankProvider::new(
            reqwest::Client::new(),
            "cf",
            "https://api.cloudflare.com/client/v4/accounts/acct123/ai/v1",
            Some("sk-test-super-secret".to_owned()),
        );
        let debug = format!("{provider:?}");
        assert!(!debug.contains("sk-test-super-secret"));
        assert!(debug.contains("<redacted>"));
    }
}
