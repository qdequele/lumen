//! Vertex AI embeddings via the `:predict` endpoint (issue #62).
//!
//! Unlike chat (where Vertex serves the exact `GenerateContent` schema of the
//! Gemini Developer API), Vertex does NOT expose `batchEmbedContents`; its
//! embeddings surface is the classic prediction API:
//! `publishers/google/models/{model}:predict` with `instances[].content` and
//! optional `parameters` (e.g. `outputDimensionality`). The response is
//! `predictions[].embeddings.{values, statistics}`, in input order, with a
//! per-input `statistics.token_count` that this module sums into upstream
//! usage (ADR 003: report upstream counts when available).
//!
//! Auth and endpoint construction reuse the chat wiring verbatim: regional
//! aiplatform host, project-scoped path, cached OAuth Bearer token.
//!
//! The batch ceiling is 1: `gemini-embedding-001` accepts a single instance
//! per request (other `text-embedding-*` models take up to 250, but the limit
//! is per-model and invisible to the gateway, so the universally safe value
//! wins). The gateway's batch splitter fans larger inputs out over concurrent
//! single-instance calls.

use async_trait::async_trait;
use lumen_core::{
    EmbedData, EmbedRequest, EmbedResponse, EmbedUsage, EmbeddingProvider, ProviderError,
};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use super::{Ready, VertexProvider};
use crate::http::post_json;

/// See the module doc: `gemini-embedding-001` takes one instance per call.
pub(super) const MAX_BATCH_SIZE: usize = 1;

/// The regional, project-scoped `:predict` URL for `model`.
fn predict_url(ready: &Ready, model: &str) -> String {
    format!(
        "{base}/v1/projects/{project}/locations/{location}/publishers/google/models/{model}:predict",
        base = ready.endpoint_base,
        project = ready.project_id,
        location = ready.location,
    )
}

// ---- Wire types ------------------------------------------------------------

#[derive(Serialize)]
struct VertexPredictRequest<'a> {
    instances: Vec<VertexInstance<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parameters: Option<VertexParameters>,
}

#[derive(Serialize)]
struct VertexInstance<'a> {
    content: &'a str,
}

#[derive(Serialize)]
struct VertexParameters {
    #[serde(rename = "outputDimensionality")]
    output_dimensionality: u32,
}

#[derive(Deserialize)]
struct VertexPredictResponse {
    #[serde(default)]
    predictions: Vec<VertexPrediction>,
}

#[derive(Deserialize)]
struct VertexPrediction {
    #[serde(default)]
    embeddings: VertexEmbeddings,
}

#[derive(Default, Deserialize)]
struct VertexEmbeddings {
    #[serde(default)]
    values: Vec<f32>,
    #[serde(default)]
    statistics: VertexStatistics,
}

#[derive(Default, Deserialize)]
struct VertexStatistics {
    #[serde(default)]
    token_count: u32,
}

/// Build the `:predict` body: one instance per text input, `dimensions`
/// mapped to `parameters.outputDimensionality`.
fn translate_request(req: &EmbedRequest) -> VertexPredictRequest<'_> {
    VertexPredictRequest {
        instances: req
            .input
            .iter()
            .map(|content| VertexInstance { content })
            .collect(),
        parameters: req.dimensions.map(|d| VertexParameters {
            output_dimensionality: d,
        }),
    }
}

/// Translate a `:predict` response into the internal OpenAI-shaped response.
/// Per-input `statistics.token_count` values are summed into upstream usage;
/// a zero sum leaves the zeroed default so the gateway derives the ADR-003
/// estimate instead.
fn translate_response(resp: VertexPredictResponse, requested_model: &str) -> EmbedResponse {
    let mut token_sum: u32 = 0;
    let data = resp
        .predictions
        .into_iter()
        .enumerate()
        .map(|(index, p)| {
            token_sum = token_sum.saturating_add(p.embeddings.statistics.token_count);
            EmbedData {
                object: "embedding".to_owned(),
                // `index` is bounded by the batch size, far below u32::MAX.
                index: u32::try_from(index).unwrap_or(u32::MAX),
                embedding: p.embeddings.values,
                encoding: lumen_core::EmbeddingEncoding::default(),
            }
        })
        .collect();

    let usage = if token_sum > 0 {
        EmbedUsage {
            prompt_tokens: token_sum,
            total_tokens: token_sum,
            estimated: None,
        }
    } else {
        EmbedUsage::default()
    };

    EmbedResponse {
        object: "list".to_owned(),
        data,
        model: requested_model.to_owned(),
        usage,
    }
}

#[async_trait]
impl EmbeddingProvider for VertexProvider {
    async fn embed(
        &self,
        req: EmbedRequest,
        cancel: CancellationToken,
    ) -> Result<EmbedResponse, ProviderError> {
        // Text-only, like the Gemini Developer API: honest 400 before any
        // upstream call (and before the token exchange).
        super::super::embed::validate_text_input(&self.provider_name, &req.input)?;

        let ready = self.ready()?;
        let token = ready.auth.token(&cancel).await?;
        let url = predict_url(ready, &req.model);
        let body = translate_request(&req);
        let bytes = post_json(
            &self.client,
            &url,
            &body,
            Some(&token),
            &self.provider_name,
            &cancel,
        )
        .await?;
        let parsed: VertexPredictResponse = serde_json::from_slice(&bytes).map_err(|e| {
            ProviderError::Translation(format!("google vertex embed response: {e}"))
        })?;
        Ok(translate_response(parsed, &req.model))
    }

    fn max_batch_size(&self) -> usize {
        MAX_BATCH_SIZE
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_core::EmbedInput;
    use serde_json::json;

    fn request(input: EmbedInput, dimensions: Option<u32>) -> EmbedRequest {
        EmbedRequest {
            model: "gemini-embedding-001".to_owned(),
            input,
            encoding_format: None,
            dimensions,
            user: None,
            extra: serde_json::Map::new(),
        }
    }

    #[test]
    fn request_maps_inputs_to_instances_and_dimensions_to_parameters() {
        let req = request(EmbedInput::Batch(vec!["a".into(), "b".into()]), Some(128));
        let body = serde_json::to_value(translate_request(&req)).unwrap();
        assert_eq!(
            body,
            json!({
                "instances": [{ "content": "a" }, { "content": "b" }],
                "parameters": { "outputDimensionality": 128 }
            })
        );
    }

    #[test]
    fn parameters_are_omitted_without_dimensions() {
        let req = request(EmbedInput::Single("x".into()), None);
        let body = serde_json::to_value(translate_request(&req)).unwrap();
        assert_eq!(body, json!({ "instances": [{ "content": "x" }] }));
    }

    #[test]
    fn response_preserves_order_and_sums_token_counts() {
        let resp = VertexPredictResponse {
            predictions: vec![
                VertexPrediction {
                    embeddings: VertexEmbeddings {
                        values: vec![0.1],
                        statistics: VertexStatistics { token_count: 3 },
                    },
                },
                VertexPrediction {
                    embeddings: VertexEmbeddings {
                        values: vec![0.2],
                        statistics: VertexStatistics { token_count: 4 },
                    },
                },
            ],
        };
        let out = translate_response(resp, "gemini-embedding-001");
        assert_eq!(out.data.len(), 2);
        assert_eq!(out.data[0].index, 0);
        assert_eq!(out.data[1].embedding, vec![0.2]);
        assert_eq!(out.usage.prompt_tokens, 7);
        assert_eq!(out.usage.total_tokens, 7);
        assert_eq!(out.usage.estimated, None);
    }

    #[test]
    fn zero_token_counts_leave_usage_zeroed_for_the_gateway_estimate() {
        let resp = VertexPredictResponse {
            predictions: vec![VertexPrediction {
                embeddings: VertexEmbeddings {
                    values: vec![1.0],
                    statistics: VertexStatistics::default(),
                },
            }],
        };
        let out = translate_response(resp, "m");
        assert_eq!(out.usage, EmbedUsage::default());
    }
}
