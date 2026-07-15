//! Ollama provider - local, keyless embeddings via `/api/embed`.
//!
//! Ollama's embed schema differs from OpenAI's: the request is
//! `{ "model", "input" }` and the response returns `embeddings` as an array of
//! vectors plus a `prompt_eval_count`. We translate both directions to the
//! internal OpenAI-shaped types.

use async_trait::async_trait;
use lumen_core::{
    EmbedData, EmbedInput, EmbedRequest, EmbedResponse, EmbedUsage, EmbeddingProvider,
    ProviderError,
};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::http::post_json;

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
    /// When `true`, reject a request that sets `dimensions` (which Ollama cannot
    /// honor) with a 400 (`LM-1001`) instead of silently dropping it.
    strict: bool,
}

impl OllamaProvider {
    /// Construct a provider pointed at `base_url` (trailing slash trimmed).
    /// `strict` controls whether an unsupported-but-meaningful field
    /// (`dimensions`) is rejected (400) or silently dropped (a `debug!` log).
    #[must_use]
    pub fn new(
        client: reqwest::Client,
        provider_name: impl Into<String>,
        base_url: impl Into<String>,
        strict: bool,
    ) -> Self {
        Self {
            client,
            provider_name: provider_name.into(),
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            strict,
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
        // Ollama's /api/embed takes a string or string array; a token-id array
        // would be forwarded verbatim and fail opaquely upstream. Honest 400
        // before any upstream call (issue #25).
        crate::mapping::reject_pretokenized_input(&self.provider_name, &req.input)?;

        // `dimensions` is meaningful (it changes the vector width) and Ollama's
        // embed API cannot honor it. In strict mode reject the request (400,
        // LM-1001) rather than silently returning full-width vectors; otherwise
        // drop it with a trace. `encoding_format` is not lost here: the gateway
        // re-encodes the output at the request edge, so it needs no handling.
        if req.dimensions.is_some() {
            if self.strict {
                return Err(ProviderError::UnsupportedField {
                    provider: self.provider_name.clone(),
                    field: "dimensions".to_owned(),
                });
            }
            tracing::debug!(
                provider = %self.provider_name,
                "dropping unsupported 'dimensions' field for Ollama embed"
            );
        }

        let url = format!("{}/api/embed", self.base_url);
        let body = OllamaEmbedRequest {
            model: &req.model,
            input: &req.input,
        };

        // Ollama is keyless; a client disconnect aborts the call (see `post_json`).
        let bytes = post_json(
            &self.client,
            &url,
            &body,
            None,
            &self.provider_name,
            &cancel,
        )
        .await?;

        let parsed: OllamaEmbedResponse = serde_json::from_slice(&bytes)
            .map_err(|e| ProviderError::Translation(format!("ollama embed response: {e}")))?;
        Ok(translate_response(parsed, &req.model))
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
            encoding: lumen_core::EmbeddingEncoding::default(),
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
            estimated: None,
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
