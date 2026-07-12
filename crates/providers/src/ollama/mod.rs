//! Ollama provider — local, keyless embeddings via `/api/embed`.
//!
//! Ollama's embed schema differs from OpenAI's: the request is
//! `{ "model", "input" }` and the response returns `embeddings` as an array of
//! vectors plus a `prompt_eval_count`. We translate both directions to the
//! internal OpenAI-shaped types.

use async_trait::async_trait;
use ferrogate_core::{
    EmbedData, EmbedInput, EmbedRequest, EmbedResponse, EmbedUsage, EmbeddingProvider,
    ProviderError,
};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::http::with_cancel;
use crate::mapping::{classify_status, parse_retry_after};

/// A conservative default batch size; Ollama has no hard documented limit but
/// is memory-bound, so we keep sub-batches modest.
const MAX_BATCH_SIZE: usize = 512;

/// An Ollama embeddings provider. Requires a `base_url` (e.g.
/// `http://localhost:11434`); no API key.
#[derive(Debug)]
pub struct OllamaProvider {
    client: reqwest::Client,
    provider_name: String,
    base_url: String,
}

impl OllamaProvider {
    /// Construct a provider pointed at `base_url` (trailing slash trimmed).
    #[must_use]
    pub fn new(
        client: reqwest::Client,
        provider_name: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Self {
        Self {
            client,
            provider_name: provider_name.into(),
            base_url: base_url.into().trim_end_matches('/').to_owned(),
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

/// Ollama `/api/embed` request. `input` matches Ollama's string-or-array shape,
/// so [`EmbedInput`]'s untagged serialization maps directly.
#[derive(Serialize)]
struct OllamaEmbedRequest<'a> {
    model: &'a str,
    input: &'a EmbedInput,
}

/// Ollama `/api/embed` response.
#[derive(Deserialize)]
struct OllamaEmbedResponse {
    #[serde(default)]
    model: String,
    embeddings: Vec<Vec<f32>>,
    #[serde(default)]
    prompt_eval_count: u32,
}

#[async_trait]
impl EmbeddingProvider for OllamaProvider {
    async fn embed(
        &self,
        req: EmbedRequest,
        cancel: CancellationToken,
    ) -> Result<EmbedResponse, ProviderError> {
        // Unsupported OpenAI-only fields are dropped with a trace, never
        // silently: Ollama's embed API has no encoding_format / dimensions.
        if req.encoding_format.is_some() || req.dimensions.is_some() {
            tracing::debug!(
                provider = %self.provider_name,
                "dropping unsupported fields (encoding_format/dimensions) for Ollama embed"
            );
        }

        let url = format!("{}/api/embed", self.base_url);
        let body = OllamaEmbedRequest {
            model: &req.model,
            input: &req.input,
        };

        let call = async {
            let response = self
                .client
                .post(&url)
                .json(&body)
                .send()
                .await
                .map_err(|e| self.map_transport(&e))?;
            let status = response.status();

            if status.is_success() {
                let bytes = response.bytes().await.map_err(|e| self.map_transport(&e))?;
                let parsed: OllamaEmbedResponse = serde_json::from_slice(&bytes).map_err(|e| {
                    ProviderError::Translation(format!("ollama embed response: {e}"))
                })?;
                Ok(translate_response(parsed, &req.model))
            } else {
                let retry_after = parse_retry_after(response.headers());
                Err(classify_status(
                    &self.provider_name,
                    status.as_u16(),
                    retry_after,
                ))
            }
        };

        with_cancel(&cancel, call).await
    }

    fn max_batch_size(&self) -> usize {
        MAX_BATCH_SIZE
    }
}

/// Translate an Ollama response into the internal OpenAI-shaped response.
fn translate_response(resp: OllamaEmbedResponse, requested_model: &str) -> EmbedResponse {
    let data = resp
        .embeddings
        .into_iter()
        .enumerate()
        .map(|(index, embedding)| EmbedData {
            object: "embedding".to_owned(),
            // `index` is bounded by the batch size, far below u32::MAX.
            index: u32::try_from(index).unwrap_or(u32::MAX),
            embedding,
        })
        .collect();

    let model = if resp.model.is_empty() {
        requested_model.to_owned()
    } else {
        resp.model
    };

    EmbedResponse {
        object: "list".to_owned(),
        data,
        model,
        usage: EmbedUsage {
            prompt_tokens: resp.prompt_eval_count,
            total_tokens: resp.prompt_eval_count,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translate_maps_embeddings_and_usage_in_order() {
        let resp = OllamaEmbedResponse {
            model: "nomic".to_owned(),
            embeddings: vec![vec![0.1, 0.2], vec![0.3, 0.4], vec![0.5, 0.6]],
            prompt_eval_count: 12,
        };
        let out = translate_response(resp, "nomic-embed");
        assert_eq!(out.object, "list");
        assert_eq!(out.model, "nomic");
        assert_eq!(out.data.len(), 3);
        assert_eq!(out.data[0].index, 0);
        assert_eq!(out.data[2].index, 2);
        assert_eq!(out.data[1].embedding, vec![0.3, 0.4]);
        assert_eq!(out.usage.total_tokens, 12);
    }

    #[test]
    fn translate_falls_back_to_requested_model_when_absent() {
        let resp = OllamaEmbedResponse {
            model: String::new(),
            embeddings: vec![vec![1.0]],
            prompt_eval_count: 0,
        };
        let out = translate_response(resp, "nomic-embed");
        assert_eq!(out.model, "nomic-embed");
    }
}
