//! Mistral provider — chat completions and embeddings.
//!
//! Both of Mistral's `chat/completions` and `embeddings` endpoints are
//! OpenAI-compatible, so this is a near-passthrough (like [`crate::openai`]).

use async_trait::async_trait;
use bytes::Bytes;
use lumen_core::{
    ChatChunk, ChatProvider, ChatRequest, ChatResponse, EmbedRequest, EmbedResponse,
    EmbeddingProvider, ProviderError,
};
use futures::stream::BoxStream;
use std::fmt;
use tokio_util::sync::CancellationToken;

use crate::chat::{enable_stream_usage, single_shot_stream};
use crate::http::{open_stream, post_json};

/// Default Mistral API base (includes the `/v1` prefix).
const DEFAULT_BASE_URL: &str = "https://api.mistral.ai/v1";

/// Conservative batch ceiling for Mistral embeddings.
const MAX_BATCH_SIZE: usize = 512;

/// A Mistral chat provider.
pub struct MistralProvider {
    client: reqwest::Client,
    provider_name: String,
    base_url: String,
    /// Bearer token. Redacted from `Debug`; never logged.
    api_key: Option<String>,
}

impl MistralProvider {
    /// Construct a provider. `base_url` defaults to the public Mistral API.
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
impl fmt::Debug for MistralProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MistralProvider")
            .field("provider_name", &self.provider_name)
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl ChatProvider for MistralProvider {
    async fn chat(
        &self,
        mut req: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<ChatResponse, ProviderError> {
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
            .map_err(|e| ProviderError::Translation(format!("mistral chat response: {e}")))
    }

    async fn chat_stream(
        &self,
        req: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, Result<ChatChunk, ProviderError>>, ProviderError> {
        let resp = self.chat(req, cancel).await?;
        Ok(single_shot_stream(resp))
    }

    async fn chat_stream_bytes(
        &self,
        mut req: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, Result<Bytes, ProviderError>>, ProviderError> {
        // Zero-copy passthrough (Mistral speaks OpenAI SSE). See ADR 004.
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

#[async_trait]
impl EmbeddingProvider for MistralProvider {
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
            .map_err(|e| ProviderError::Translation(format!("mistral embeddings response: {e}")))
    }

    fn max_batch_size(&self) -> usize {
        MAX_BATCH_SIZE
    }
}
