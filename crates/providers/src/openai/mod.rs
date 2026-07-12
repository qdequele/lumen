//! OpenAI provider — the canonical reference implementation.
//!
//! For embeddings the OpenAI wire schema is identical to our internal
//! [`EmbedRequest`]/[`EmbedResponse`], so translation is a near-passthrough.
//! Later capabilities (chat, M4) will add a `translate.rs`.

use async_trait::async_trait;
use ferrogate_core::{EmbedRequest, EmbedResponse, EmbeddingProvider, ProviderError};
use std::fmt;
use tokio_util::sync::CancellationToken;

use crate::http::with_cancel;
use crate::mapping::{classify_status, parse_retry_after};

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

    fn map_transport(&self, err: &reqwest::Error) -> ProviderError {
        if err.is_timeout() {
            ProviderError::Timeout {
                provider: self.provider_name.clone(),
            }
        } else {
            ProviderError::Unavailable {
                provider: self.provider_name.clone(),
            }
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

        let call = async {
            let mut builder = self.client.post(&url).json(&req);
            if let Some(key) = &self.api_key {
                builder = builder.bearer_auth(key);
            }

            let response = builder.send().await.map_err(|e| self.map_transport(&e))?;
            let status = response.status();

            if status.is_success() {
                let bytes = response.bytes().await.map_err(|e| self.map_transport(&e))?;
                // The detail stays out of the client response (from_provider
                // discards it) so a malformed body cannot leak upstream data.
                serde_json::from_slice::<EmbedResponse>(&bytes).map_err(|e| {
                    ProviderError::Translation(format!("openai embeddings response: {e}"))
                })
            } else {
                let retry_after = parse_retry_after(response.headers());
                Err(classify_status(
                    &self.provider_name,
                    status.as_u16(),
                    retry_after,
                ))
            }
        };

        // Client disconnect aborts the in-flight HTTP call (see `with_cancel`).
        with_cancel(&cancel, call).await
    }

    fn max_batch_size(&self) -> usize {
        MAX_BATCH_SIZE
    }
}
