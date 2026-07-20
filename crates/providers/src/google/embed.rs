//! Gemini embeddings via `batchEmbedContents` (issue #62).
//!
//! The Gemini Developer API embeds batches through
//! `models/{model}:batchEmbedContents`: the body carries one inner request per
//! input (`requests[].content.parts[].text`), each inner request repeating the
//! URL's model as `models/{model}`. The response is `embeddings[].values`, in
//! input order. `usageMetadata.promptTokenCount` is parsed when the upstream
//! returns it; when absent the usage stays zeroed and the gateway derives the
//! ADR-003 local estimate marked `estimated` at the request edge (exactly as
//! for TEI).
//!
//! The API is text-only: pre-tokenized token-id arrays and image content parts
//! are rejected with an honest client error before any upstream call.

use async_trait::async_trait;
use lumen_core::{
    EmbedData, EmbedInput, EmbedRequest, EmbedResponse, EmbedUsage, EmbeddingProvider,
    ProviderError,
};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use super::GoogleProvider;
use crate::http::post_json_with_headers;

/// Gemini's documented ceiling for `batchEmbedContents` is 100 inner requests
/// per call; the gateway splits larger inputs (`crate::batch`).
pub(super) const MAX_BATCH_SIZE: usize = 100;

/// Reject input shapes the Gemini/Vertex embedding APIs cannot consume, BEFORE
/// any upstream call (rule 8: an honest 400, never an opaque upstream error).
/// Shared by the `google` and `vertex_ai` kinds: both are text-only.
///
/// # Errors
/// [`ProviderError::UnsupportedInput`] for pre-tokenized token-id arrays and
/// for multimodal items carrying an image part.
pub(super) fn validate_text_input(provider: &str, input: &EmbedInput) -> Result<(), ProviderError> {
    crate::mapping::reject_pretokenized_input(provider, input)?;
    if input.has_image() {
        return Err(ProviderError::UnsupportedInput {
            provider: provider.to_owned(),
            reason: "image input (text-only embeddings API)".to_owned(),
        });
    }
    Ok(())
}

// ---- Wire types ------------------------------------------------------------

#[derive(Serialize)]
struct GeminiBatchEmbedRequest<'a> {
    requests: Vec<GeminiEmbedContentRequest<'a>>,
}

/// One inner `EmbedContentRequest`. Gemini requires `model` to repeat the
/// URL's model as a `models/{model}` resource path on every inner request.
#[derive(Serialize)]
struct GeminiEmbedContentRequest<'a> {
    model: String,
    content: GeminiEmbedContent<'a>,
    #[serde(
        rename = "outputDimensionality",
        skip_serializing_if = "Option::is_none"
    )]
    output_dimensionality: Option<u32>,
}

#[derive(Serialize)]
struct GeminiEmbedContent<'a> {
    parts: [GeminiEmbedTextPart<'a>; 1],
}

#[derive(Serialize)]
struct GeminiEmbedTextPart<'a> {
    text: std::borrow::Cow<'a, str>,
}

#[derive(Deserialize)]
struct GeminiBatchEmbedResponse {
    #[serde(default)]
    embeddings: Vec<GeminiEmbedding>,
    /// Present on newer API surfaces; absent responses fall back to the
    /// gateway's ADR-003 estimate (usage stays zeroed here).
    #[serde(rename = "usageMetadata", default)]
    usage_metadata: Option<GeminiEmbedUsage>,
}

#[derive(Deserialize)]
struct GeminiEmbedding {
    #[serde(default)]
    values: Vec<f32>,
}

#[derive(Deserialize)]
struct GeminiEmbedUsage {
    #[serde(rename = "promptTokenCount", default)]
    prompt_token_count: u32,
}

/// Build the `batchEmbedContents` body: one inner request per input ITEM (a
/// `Multi` `Parts` item joins its text fragments), so the inner-request count
/// equals `input.len()` and stays within Gemini's 100-per-call ceiling after
/// the gateway's split (issue #90).
fn translate_request(req: &EmbedRequest) -> GeminiBatchEmbedRequest<'_> {
    let model = format!("models/{}", req.model);
    GeminiBatchEmbedRequest {
        requests: req
            .input
            .item_texts()
            .into_iter()
            .map(|text| GeminiEmbedContentRequest {
                model: model.clone(),
                content: GeminiEmbedContent {
                    parts: [GeminiEmbedTextPart { text }],
                },
                output_dimensionality: req.dimensions,
            })
            .collect(),
    }
}

/// Translate a Gemini batch response into the internal OpenAI-shaped response.
/// Upstream usage is reported verbatim when present; otherwise the zeroed
/// default lets the gateway derive the ADR-003 estimate.
fn translate_response(resp: GeminiBatchEmbedResponse, requested_model: &str) -> EmbedResponse {
    let data = resp
        .embeddings
        .into_iter()
        .enumerate()
        .map(|(index, e)| EmbedData {
            object: "embedding".to_owned(),
            // `index` is bounded by the batch size, far below u32::MAX.
            index: u32::try_from(index).unwrap_or(u32::MAX),
            embedding: e.values,
            encoding: lumen_core::EmbeddingEncoding::default(),
        })
        .collect();

    let usage = match resp.usage_metadata {
        Some(u) if u.prompt_token_count > 0 => EmbedUsage {
            prompt_tokens: u.prompt_token_count,
            total_tokens: u.prompt_token_count,
            estimated: None,
        },
        _ => EmbedUsage::default(),
    };

    EmbedResponse {
        object: "list".to_owned(),
        data,
        model: requested_model.to_owned(),
        usage,
    }
}

#[async_trait]
impl EmbeddingProvider for GoogleProvider {
    async fn embed(
        &self,
        req: EmbedRequest,
        cancel: CancellationToken,
    ) -> Result<EmbedResponse, ProviderError> {
        validate_text_input(&self.provider_name, &req.input)?;

        // The model is part of the path; the key is a header, never the URL.
        let url = format!(
            "{}/v1beta/models/{}:batchEmbedContents",
            self.base_url, req.model
        );
        let body = translate_request(&req);
        let headers = [("x-goog-api-key", self.api_key.as_deref().unwrap_or(""))];

        let bytes = post_json_with_headers(
            &self.client,
            &url,
            &body,
            &headers,
            &self.provider_name,
            &cancel,
        )
        .await?;

        let parsed: GeminiBatchEmbedResponse = serde_json::from_slice(&bytes)
            .map_err(|e| ProviderError::Translation(format!("google embed response: {e}")))?;
        Ok(translate_response(parsed, &req.model))
    }

    fn max_batch_size(&self) -> usize {
        MAX_BATCH_SIZE
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn request_carries_one_inner_request_per_input_with_model_path() {
        let req = request(EmbedInput::Batch(vec!["a".into(), "b".into()]), None);
        let body = serde_json::to_value(translate_request(&req)).unwrap();
        assert_eq!(
            body,
            json!({
                "requests": [
                    {
                        "model": "models/gemini-embedding-001",
                        "content": { "parts": [{ "text": "a" }] }
                    },
                    {
                        "model": "models/gemini-embedding-001",
                        "content": { "parts": [{ "text": "b" }] }
                    }
                ]
            })
        );
    }

    #[test]
    fn multi_part_item_yields_one_inner_request_per_item() {
        // A Multi with a two-text-part item plus a bare-text item: two ITEMS,
        // so two inner requests (NOT three fragment-level requests). Keeps the
        // inner-request count at input.len(), within the 100-per-call ceiling
        // after splitting (issue #90).
        use lumen_core::{ContentPart, EmbedItem};
        let input = EmbedInput::Multi(vec![
            EmbedItem::Parts(vec![
                ContentPart {
                    kind: "text".to_owned(),
                    text: Some("foo".to_owned()),
                    image_url: None,
                    extra: serde_json::Map::new(),
                },
                ContentPart {
                    kind: "text".to_owned(),
                    text: Some("bar".to_owned()),
                    image_url: None,
                    extra: serde_json::Map::new(),
                },
            ]),
            EmbedItem::Text("baz".to_owned()),
        ]);
        let req = request(input, None);
        let built = translate_request(&req);
        assert_eq!(built.requests.len(), 2);
        let body = serde_json::to_value(built).unwrap();
        // Two text fragments join with a newline (never fused into "foobar").
        assert_eq!(
            body["requests"][0]["content"]["parts"][0]["text"],
            "foo\nbar"
        );
        assert_eq!(body["requests"][1]["content"]["parts"][0]["text"], "baz");
    }

    #[test]
    fn dimensions_maps_to_output_dimensionality() {
        let req = request(EmbedInput::Single("x".into()), Some(64));
        let body = serde_json::to_value(translate_request(&req)).unwrap();
        assert_eq!(body["requests"][0]["outputDimensionality"], 64);
    }

    #[test]
    fn response_preserves_order_and_reports_upstream_usage() {
        let resp = GeminiBatchEmbedResponse {
            embeddings: vec![
                GeminiEmbedding {
                    values: vec![0.1, 0.2],
                },
                GeminiEmbedding { values: vec![0.3] },
            ],
            usage_metadata: Some(GeminiEmbedUsage {
                prompt_token_count: 9,
            }),
        };
        let out = translate_response(resp, "gemini-embedding-001");
        assert_eq!(out.object, "list");
        assert_eq!(out.model, "gemini-embedding-001");
        assert_eq!(out.data.len(), 2);
        assert_eq!(out.data[0].index, 0);
        assert_eq!(out.data[1].index, 1);
        assert_eq!(out.data[0].embedding, vec![0.1, 0.2]);
        assert_eq!(out.usage.prompt_tokens, 9);
        assert_eq!(out.usage.total_tokens, 9);
        assert_eq!(out.usage.estimated, None);
    }

    #[test]
    fn absent_usage_metadata_leaves_usage_zeroed_for_the_gateway_estimate() {
        let resp = GeminiBatchEmbedResponse {
            embeddings: vec![GeminiEmbedding { values: vec![1.0] }],
            usage_metadata: None,
        };
        let out = translate_response(resp, "gemini-embedding-001");
        assert_eq!(out.usage, EmbedUsage::default());
    }

    #[test]
    fn validate_rejects_tokens_and_images_but_accepts_text() {
        assert!(validate_text_input("google", &EmbedInput::Tokens(vec![1, 2])).is_err());
        assert!(validate_text_input("google", &EmbedInput::TokenBatch(vec![vec![1]])).is_err());
        assert!(validate_text_input("google", &EmbedInput::Single("hi".into())).is_ok());
        assert!(
            validate_text_input("google", &EmbedInput::Batch(vec!["a".into(), "b".into()])).is_ok()
        );

        let with_image = EmbedInput::Multi(vec![lumen_core::EmbedItem::Parts(vec![
            lumen_core::ContentPart {
                kind: "image_url".to_owned(),
                text: None,
                image_url: Some(lumen_core::ImageUrl {
                    url: "data:image/png;base64,QUJD".to_owned(),
                    detail: None,
                }),
                extra: serde_json::Map::new(),
            },
        ])]);
        match validate_text_input("google", &with_image) {
            Err(ProviderError::UnsupportedInput { provider, .. }) => {
                assert_eq!(provider, "google");
            }
            other => panic!("expected UnsupportedInput, got {other:?}"),
        }
    }
}
