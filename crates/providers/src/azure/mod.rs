//! Azure OpenAI provider - deployment routing + `api-version`.
//!
//! Azure OpenAI reuses the OpenAI request/response wire schema verbatim (this
//! module is a near-passthrough, like [`crate::openai`]), but its API shape
//! differs from public OpenAI in three ways this module bridges:
//!
//! * the URL encodes the **deployment**, not the model:
//!   `{endpoint}/openai/deployments/{deployment}/chat/completions?api-version=...`
//!   (same shape for `/embeddings`). Azure routes purely on the URL path; the
//!   `model` field carried in the request body is accepted but ignored by
//!   Azure for routing purposes.
//! * every request carries an `api-version` query parameter (a dated string,
//!   e.g. `2024-10-21`), never a header or a path segment.
//! * auth is the `api-key` header, never a bearer token.
//!
//! **Deployment routing.** LUMEN already resolves `(capability, model id)` to
//! an `upstream_id` before calling a provider, and rewrites `req.model` to
//! that `upstream_id` on every attempt (see `crates/server/src/chat.rs` and
//! `embeddings.rs`). Azure deployment routing therefore needs no dedicated
//! config field: set a model's `upstream_id` to the Azure **deployment
//! name**, and by the time this provider runs, `req.model` already carries it.
//!
//! **`api-version`.** [`crate::registry::ProviderSpec`] has no dedicated
//! `api_version` field today - adding one needs a matching `crates/server`
//! config change, which is out of this crate's scope (flagged in the
//! provider-integrator report). Until then, the operator selects the
//! `api-version` via the provider's `base_url`: append
//! `?api-version=YYYY-MM-DD` and it is used verbatim on every request; omit
//! it and [`DEFAULT_API_VERSION`] applies.

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use lumen_core::{
    ChatChunk, ChatProvider, ChatRequest, ChatResponse, EmbedRequest, EmbedResponse,
    EmbeddingProvider, ProviderError,
};
use std::fmt;
use tokio_util::sync::CancellationToken;

use crate::chat::{enable_stream_usage, single_shot_stream};
use crate::http::{open_stream_with_headers, post_json_with_headers};

/// Azure OpenAI `api-version` used when the configured `base_url` carries
/// none. Overridable per provider: append `?api-version=YYYY-MM-DD` to
/// `base_url`. Pinned rather than "latest" so a given LUMEN build's upstream
/// wire shape never shifts under an operator without a deliberate config
/// change.
const DEFAULT_API_VERSION: &str = "2024-10-21";

/// Azure OpenAI's documented maximum number of inputs per embeddings request
/// (the same array-size ceiling as the OpenAI embedding models it hosts).
const MAX_BATCH_SIZE: usize = 2048;

/// An Azure OpenAI provider: deployment-routed URLs, `api-version`, `api-key`
/// auth.
pub struct AzureProvider {
    client: reqwest::Client,
    /// The configured provider name, used to attribute upstream errors.
    provider_name: String,
    /// Resource endpoint, e.g. `https://my-resource.openai.azure.com`
    /// (scheme + host + path only; any query string from `base_url`, e.g.
    /// `?api-version=...`, is stripped here and parsed into `api_version`).
    endpoint: String,
    api_version: String,
    /// Sent as the `api-key` header, never a bearer token. Redacted from
    /// `Debug`; never logged.
    api_key: Option<String>,
}

impl AzureProvider {
    /// Construct a provider. `base_url` is the Azure resource endpoint
    /// (`https://<resource>.openai.azure.com`), optionally carrying an
    /// `?api-version=...` override. Required: unlike the public OpenAI kind,
    /// Azure has no shared default endpoint - every resource is operator
    /// specific.
    #[must_use]
    pub fn new(
        client: reqwest::Client,
        provider_name: impl Into<String>,
        base_url: &str,
        api_key: Option<String>,
    ) -> Self {
        let (endpoint, api_version) = split_endpoint_and_version(base_url);
        Self {
            client,
            provider_name: provider_name.into(),
            endpoint,
            api_version,
            api_key,
        }
    }

    /// `{endpoint}/openai/deployments/{deployment}/{path}?api-version={version}`.
    /// `deployment` is `req.model`: the router already rewrote it to the
    /// model's `upstream_id` before calling this provider.
    fn deployment_url(&self, deployment: &str, path: &str) -> String {
        format!(
            "{}/openai/deployments/{deployment}/{path}?api-version={}",
            self.endpoint, self.api_version
        )
    }

    /// The single `api-key` auth header (never a bearer token).
    fn headers(&self) -> [(&str, &str); 1] {
        [("api-key", self.api_key.as_deref().unwrap_or(""))]
    }
}

/// Split a configured `base_url` into its bare endpoint (scheme + host,
/// trailing slash and any query string stripped) and its `api-version`,
/// taken from an `?api-version=...` query parameter if present, else
/// [`DEFAULT_API_VERSION`].
fn split_endpoint_and_version(base_url: &str) -> (String, String) {
    let trimmed = base_url.trim_end_matches('/');
    let Some((endpoint, query)) = trimmed.split_once('?') else {
        return (trimmed.to_owned(), DEFAULT_API_VERSION.to_owned());
    };
    let version = query
        .split('&')
        .find_map(|pair| pair.strip_prefix("api-version="))
        .filter(|v| !v.is_empty())
        .unwrap_or(DEFAULT_API_VERSION)
        .to_owned();
    (endpoint.trim_end_matches('/').to_owned(), version)
}

/// Redacted so the API key can never reach a log line via `{:?}`.
impl fmt::Debug for AzureProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AzureProvider")
            .field("provider_name", &self.provider_name)
            .field("endpoint", &self.endpoint)
            .field("api_version", &self.api_version)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            // `client` is intentionally omitted.
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl ChatProvider for AzureProvider {
    async fn chat(
        &self,
        mut req: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<ChatResponse, ProviderError> {
        // This entry point is non-streaming; never ask the upstream to stream.
        req.stream = false;
        let url = self.deployment_url(&req.model, "chat/completions");
        let bytes = post_json_with_headers(
            &self.client,
            &url,
            &req,
            &self.headers(),
            &self.provider_name,
            &cancel,
        )
        .await?;
        serde_json::from_slice::<ChatResponse>(&bytes)
            .map_err(|e| ProviderError::Translation(format!("azure chat response: {e}")))
    }

    async fn chat_stream(
        &self,
        req: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, Result<ChatChunk, ProviderError>>, ProviderError> {
        // Typed fallback (rarely used: the server streams via chat_stream_bytes).
        let resp = self.chat(req, cancel).await?;
        Ok(single_shot_stream(resp))
    }

    async fn chat_stream_bytes(
        &self,
        mut req: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, Result<Bytes, ProviderError>>, ProviderError> {
        // Zero-copy passthrough: Azure OpenAI speaks OpenAI SSE verbatim, so
        // forward the upstream body bytes as-is (framing + `[DONE]`), no
        // per-chunk serde round trip. See ADR 004.
        enable_stream_usage(&mut req);
        let url = self.deployment_url(&req.model, "chat/completions");
        open_stream_with_headers(
            &self.client,
            &url,
            &req,
            &self.headers(),
            &self.provider_name,
            &cancel,
        )
        .await
    }
}

#[async_trait]
impl EmbeddingProvider for AzureProvider {
    async fn embed(
        &self,
        req: EmbedRequest,
        cancel: CancellationToken,
    ) -> Result<EmbedResponse, ProviderError> {
        let url = self.deployment_url(&req.model, "embeddings");
        let bytes = post_json_with_headers(
            &self.client,
            &url,
            &req,
            &self.headers(),
            &self.provider_name,
            &cancel,
        )
        .await?;
        serde_json::from_slice::<EmbedResponse>(&bytes)
            .map_err(|e| ProviderError::Translation(format!("azure embeddings response: {e}")))
    }

    fn max_batch_size(&self) -> usize {
        MAX_BATCH_SIZE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_and_version_split_from_a_query_string() {
        let (endpoint, version) = split_endpoint_and_version(
            "https://my-resource.openai.azure.com/?api-version=2024-06-01",
        );
        assert_eq!(endpoint, "https://my-resource.openai.azure.com");
        assert_eq!(version, "2024-06-01");
    }

    #[test]
    fn missing_query_falls_back_to_the_default_version() {
        let (endpoint, version) =
            split_endpoint_and_version("https://my-resource.openai.azure.com");
        assert_eq!(endpoint, "https://my-resource.openai.azure.com");
        assert_eq!(version, DEFAULT_API_VERSION);
    }

    #[test]
    fn deployment_url_uses_the_request_model_as_the_deployment() {
        let provider = AzureProvider::new(
            reqwest::Client::new(),
            "azure-test",
            "https://my-resource.openai.azure.com",
            Some("sk-test-xxx".to_owned()),
        );
        let url = provider.deployment_url("my-gpt4o-deployment", "chat/completions");
        assert_eq!(
            url,
            "https://my-resource.openai.azure.com/openai/deployments/my-gpt4o-deployment/chat/completions?api-version=2024-10-21"
        );
    }

    #[test]
    fn debug_never_leaks_the_api_key() {
        let provider = AzureProvider::new(
            reqwest::Client::new(),
            "azure-test",
            "https://my-resource.openai.azure.com",
            Some("sk-test-xxx".to_owned()),
        );
        let dbg = format!("{provider:?}");
        assert!(!dbg.contains("sk-test-xxx"));
        assert!(dbg.contains("<redacted>"));
    }
}
