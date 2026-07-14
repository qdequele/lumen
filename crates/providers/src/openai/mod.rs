//! OpenAI provider - the canonical reference implementation.
//!
//! For embeddings the OpenAI wire schema is identical to our internal
//! [`EmbedRequest`]/[`EmbedResponse`], so translation is a near-passthrough.
//! Later capabilities (chat, M4) will add a `translate.rs`.

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
use crate::http::{open_stream, post_json};

/// Default OpenAI API base (includes the `/v1` prefix).
const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

/// OpenAI's documented maximum number of inputs per embeddings request.
const MAX_BATCH_SIZE: usize = 2048;

/// An OpenAI-compatible embeddings provider.
///
/// Also serves any OpenAI-compatible endpoint (vLLM, LiteLLM, etc.) via a
/// custom `base_url`.
pub struct OpenAiProvider {
    client: reqwest::Client,
    /// The configured provider name, used to attribute upstream errors.
    provider_name: String,
    base_url: String,
    /// Bearer token. Redacted from `Debug`; never logged.
    api_key: Option<String>,
}

impl OpenAiProvider {
    /// Construct a provider. `base_url` defaults to the public OpenAI API.
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
impl fmt::Debug for OpenAiProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenAiProvider")
            .field("provider_name", &self.provider_name)
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            // `client` is intentionally omitted.
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl EmbeddingProvider for OpenAiProvider {
    async fn embed(
        &self,
        req: EmbedRequest,
        cancel: CancellationToken,
    ) -> Result<EmbedResponse, ProviderError> {
        let url = format!("{}/embeddings", self.base_url);

        // Shared transport + error classification; a client disconnect aborts
        // the in-flight call (see `post_json`).
        let bytes = post_json(
            &self.client,
            &url,
            &req,
            self.api_key.as_deref(),
            &self.provider_name,
            &cancel,
        )
        .await?;

        // The detail stays out of the client response (from_provider discards
        // it) so a malformed body cannot leak upstream data.
        serde_json::from_slice::<EmbedResponse>(&bytes)
            .map_err(|e| ProviderError::Translation(format!("openai embeddings response: {e}")))
    }

    fn max_batch_size(&self) -> usize {
        MAX_BATCH_SIZE
    }
}

#[async_trait]
impl ChatProvider for OpenAiProvider {
    async fn chat(
        &self,
        mut req: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<ChatResponse, ProviderError> {
        // This entry point is non-streaming; never ask the upstream to stream.
        req.stream = false;
        let url = format!("{}/chat/completions", self.base_url);
        let bytes = post_json(
            &self.client,
            &url,
            &req,
            self.api_key.as_deref(),
            &self.provider_name,
            &cancel,
        )
        .await?;
        serde_json::from_slice::<ChatResponse>(&bytes)
            .map_err(|e| ProviderError::Translation(format!("openai chat response: {e}")))
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
        // Zero-copy passthrough: OpenAI already speaks OpenAI SSE, so forward
        // the upstream body bytes verbatim (framing + `[DONE]`), no per-chunk
        // serde round trip. See ADR 004.
        enable_stream_usage(&mut req);
        let url = format!("{}/chat/completions", self.base_url);
        open_stream(
            &self.client,
            &url,
            &req,
            self.api_key.as_deref(),
            &self.provider_name,
            &cancel,
        )
        .await
    }
}
