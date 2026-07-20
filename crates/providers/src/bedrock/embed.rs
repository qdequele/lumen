//! AWS Bedrock embeddings via the per-model `InvokeModel` API (issue #95).
//!
//! Bedrock has no `Converse`-style uniform schema for embeddings, so each model
//! family carries its own request/response shape on `POST /model/{id}/invoke`.
//! Two families are supported, routed by the model id:
//!
//! - **Amazon Titan** (`amazon.titan-embed-text-v2:0` and predecessors): embeds
//!   ONE text per call (`{ "inputText": "..." }`), returning
//!   `{ "embedding": [...], "inputTextTokenCount": N }`. The gateway loops one
//!   signed request per input and reassembles the batch in order. Titan v2
//!   honors `dimensions` (mapped to Titan's `dimensions`/`normalize`); older
//!   Titan models do not, so `dimensions` is dropped (or rejected in strict
//!   mode).
//! - **Cohere Embed on Bedrock** (`cohere.embed-english-v3`,
//!   `cohere.embed-multilingual-v3`): embeds a batch in one call
//!   (`{ "texts": [...], "input_type": "..." }`), returning
//!   `{ "embeddings": [[...]] }` (or `{ "embeddings": { "float": [[...]] } }`).
//!   `input_type` defaults to `search_document` (the indexing case) and honors
//!   a caller override from `EmbedRequest::extra`. Cohere does not accept
//!   `dimensions`.
//!
//! Token accounting follows ADR 003: Titan's per-call `inputTextTokenCount` and
//! Cohere's `x-amzn-bedrock-input-token-count` response header are reported as
//! upstream usage; when neither is present the usage stays zeroed so the request
//! edge derives the local estimate (never a silent zero).
//!
//! Both families are text-only: pre-tokenized token-id arrays and image content
//! parts are rejected with an honest client error before any upstream call.
//!
//! SigV4 signing, region resolution, cancellation and error classification are
//! reused verbatim from the chat path ([`super::BedrockProvider`]); only the
//! body translation differs.

use async_trait::async_trait;
use bytes::Bytes;
use lumen_core::{
    EmbedData, EmbedRequest, EmbedResponse, EmbedUsage, EmbeddingEncoding, EmbeddingProvider,
    ProviderError,
};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use super::BedrockProvider;
use crate::http::{map_transport, with_cancel};
use crate::mapping::{classify_status, parse_retry_after, reject_pretokenized_input};

/// Both Bedrock embedding families have per-call input limits that differ
/// (Titan: exactly one text; Cohere: 96), and a single `BedrockProvider` serves
/// any model. Reporting the conservative floor of `1` lets the router split
/// every batch into single-input sub-batches it runs concurrently, which is
/// correct for both families (Titan's hard limit, and a safe under-use of
/// Cohere's larger ceiling). `embed` itself still handles a multi-input request
/// directly (looping for Titan, one call for Cohere), so a caller that bypasses
/// the router is served correctly too.
const MAX_BATCH_SIZE: usize = 1;

/// Which Bedrock embedding wire schema a model id maps to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EmbedFamily {
    Titan,
    Cohere,
}

/// Resolve the embedding wire family from the model id. Recognises the
/// `amazon.titan-embed-*` and `cohere.embed-*` families (tolerating a
/// cross-region inference-profile prefix such as `us.`), case-insensitively.
///
/// # Errors
/// [`ProviderError::Translation`] when the model id names no known Bedrock
/// embedding family (a config/routing mismatch, deterministic - not retried).
fn resolve_family(provider: &str, model: &str) -> Result<EmbedFamily, ProviderError> {
    let m = model.to_ascii_lowercase();
    if m.contains("titan-embed") {
        Ok(EmbedFamily::Titan)
    } else if m.contains("cohere.embed")
        || m.contains("embed-english")
        || m.contains("embed-multilingual")
    {
        Ok(EmbedFamily::Cohere)
    } else {
        Err(ProviderError::Translation(format!(
            "bedrock provider '{provider}': model '{model}' is not a known embedding \
             model (expected an amazon.titan-embed-* or cohere.embed-* id)"
        )))
    }
}

/// Reject input shapes the Bedrock embedding APIs cannot consume, BEFORE any
/// upstream call (rule 8: an honest 400, never an opaque upstream error). Both
/// families are text-only.
fn validate_text_input(provider: &str, req: &EmbedRequest) -> Result<(), ProviderError> {
    reject_pretokenized_input(provider, &req.input)?;
    if req.input.has_image() {
        return Err(ProviderError::UnsupportedInput {
            provider: provider.to_owned(),
            reason: "image input (text-only embeddings API)".to_owned(),
        });
    }
    Ok(())
}

/// Handle a `dimensions` request the target family cannot honor: reject it in
/// strict mode (400, `LM-1001`), else drop it with a `debug!` trace (the Ollama
/// precedent, issue #25).
fn handle_unsupported_dimensions(
    provider: &str,
    strict: bool,
    dimensions: Option<u32>,
) -> Result<(), ProviderError> {
    if dimensions.is_some() {
        if strict {
            return Err(ProviderError::UnsupportedField {
                provider: provider.to_owned(),
                field: "dimensions".to_owned(),
            });
        }
        tracing::debug!(
            provider,
            "dropping `dimensions`: this bedrock embedding model cannot honor it"
        );
    }
    Ok(())
}

// ---- Titan wire types ------------------------------------------------------

#[derive(Serialize)]
struct TitanEmbedRequest<'a> {
    #[serde(rename = "inputText")]
    input_text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    dimensions: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    normalize: Option<bool>,
}

#[derive(Deserialize)]
struct TitanEmbedResponse {
    #[serde(default)]
    embedding: Vec<f32>,
    #[serde(rename = "inputTextTokenCount", default)]
    input_text_token_count: u32,
}

// ---- Cohere-on-Bedrock wire types ------------------------------------------

#[derive(Serialize)]
struct CohereEmbedRequest<'a> {
    texts: Vec<&'a str>,
    input_type: &'a str,
}

#[derive(Deserialize)]
struct CohereEmbedResponse {
    #[serde(default)]
    embeddings: CohereEmbeddings,
}

/// Cohere returns embeddings either as a bare array of vectors, or - when
/// `embedding_types` was requested - as an object keyed by type. Accept both.
#[derive(Deserialize)]
#[serde(untagged)]
enum CohereEmbeddings {
    Array(Vec<Vec<f32>>),
    ByType {
        #[serde(default)]
        float: Vec<Vec<f32>>,
    },
}

impl Default for CohereEmbeddings {
    fn default() -> Self {
        CohereEmbeddings::Array(Vec::new())
    }
}

impl CohereEmbeddings {
    fn into_vectors(self) -> Vec<Vec<f32>> {
        match self {
            CohereEmbeddings::Array(v) | CohereEmbeddings::ByType { float: v } => v,
        }
    }
}

/// Build one [`EmbedData`] per vector, indexed in order.
fn to_embed_data(vectors: Vec<Vec<f32>>) -> Vec<EmbedData> {
    vectors
        .into_iter()
        .enumerate()
        .map(|(index, embedding)| EmbedData {
            object: "embedding".to_owned(),
            // `index` is bounded by the batch size, far below u32::MAX.
            index: u32::try_from(index).unwrap_or(u32::MAX),
            embedding,
            encoding: EmbeddingEncoding::default(),
        })
        .collect()
}

/// Turn an upstream input-token count into ADR-003 usage: reported verbatim
/// when present (`estimated: None`), else zeroed so the edge estimates.
fn usage_from_input_tokens(input_tokens: u32) -> EmbedUsage {
    if input_tokens > 0 {
        EmbedUsage {
            prompt_tokens: input_tokens,
            total_tokens: input_tokens,
            estimated: None,
        }
    } else {
        EmbedUsage::default()
    }
}

impl BedrockProvider {
    /// Send a signed `InvokeModel` request for `model`, honouring `cancel`, and
    /// return the response body plus the upstream input-token count from the
    /// `x-amzn-bedrock-input-token-count` header when present. Reuses the chat
    /// path's SigV4 signing ([`Self::signed_request`]) and error
    /// classification.
    async fn invoke_model(
        &self,
        model: &str,
        body_bytes: Vec<u8>,
        cancel: &CancellationToken,
    ) -> Result<(Bytes, Option<u32>), ProviderError> {
        let path = Self::path(model, "invoke");
        let builder = self.signed_request(&path, body_bytes)?;
        let provider = self.provider_name.as_str();
        let call = async {
            let response = builder
                .send()
                .await
                .map_err(|e| map_transport(provider, &e))?;
            let status = response.status();
            if status.is_success() {
                let input_tokens = response
                    .headers()
                    .get("x-amzn-bedrock-input-token-count")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.trim().parse::<u32>().ok());
                let bytes = response
                    .bytes()
                    .await
                    .map_err(|e| map_transport(provider, &e))?;
                Ok((bytes, input_tokens))
            } else {
                let retry_after = parse_retry_after(response.headers());
                Err(classify_status(provider, status.as_u16(), retry_after))
            }
        };
        with_cancel(cancel, call).await
    }

    /// Titan embeds one text per call: loop a signed `InvokeModel` per input,
    /// preserving order and summing `inputTextTokenCount` as upstream usage.
    async fn embed_titan(
        &self,
        req: EmbedRequest,
        cancel: CancellationToken,
    ) -> Result<EmbedResponse, ProviderError> {
        let provider = self.provider_name.clone();
        // Titan v2 honors output dimensions; earlier Titan models do not.
        let supports_dimensions = req.model.to_ascii_lowercase().contains("v2");
        if !supports_dimensions {
            handle_unsupported_dimensions(&provider, self.strict, req.dimensions)?;
        }
        let dimensions = if supports_dimensions {
            req.dimensions
        } else {
            None
        };
        let normalize = dimensions.map(|_| true);

        let mut data: Vec<EmbedData> = Vec::new();
        let mut total_tokens: u32 = 0;
        let mut any_usage = false;
        for text in req.input.iter() {
            let body = serde_json::to_vec(&TitanEmbedRequest {
                input_text: text,
                dimensions,
                normalize,
            })
            .map_err(|e| ProviderError::Translation(format!("bedrock titan request: {e}")))?;
            let (bytes, _header_tokens) = self.invoke_model(&req.model, body, &cancel).await?;
            let parsed: TitanEmbedResponse = serde_json::from_slice(&bytes)
                .map_err(|e| ProviderError::Translation(format!("bedrock titan response: {e}")))?;
            if parsed.input_text_token_count > 0 {
                any_usage = true;
                total_tokens = total_tokens.saturating_add(parsed.input_text_token_count);
            }
            data.push(EmbedData {
                object: "embedding".to_owned(),
                index: u32::try_from(data.len()).unwrap_or(u32::MAX),
                embedding: parsed.embedding,
                encoding: EmbeddingEncoding::default(),
            });
        }

        let usage = if any_usage {
            usage_from_input_tokens(total_tokens)
        } else {
            EmbedUsage::default()
        };
        Ok(EmbedResponse {
            object: "list".to_owned(),
            data,
            model: req.model,
            usage,
        })
    }

    /// Cohere on Bedrock embeds a batch in one call. The input token count comes
    /// from the `x-amzn-bedrock-input-token-count` response header.
    async fn embed_cohere(
        &self,
        req: EmbedRequest,
        cancel: CancellationToken,
    ) -> Result<EmbedResponse, ProviderError> {
        let provider = self.provider_name.clone();
        handle_unsupported_dimensions(&provider, self.strict, req.dimensions)?;

        // `input_type` defaults to the indexing case; honor a caller override.
        let input_type = req
            .extra
            .get("input_type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("search_document");
        let texts: Vec<&str> = req.input.iter().collect();
        let body = serde_json::to_vec(&CohereEmbedRequest { texts, input_type })
            .map_err(|e| ProviderError::Translation(format!("bedrock cohere request: {e}")))?;

        let (bytes, header_tokens) = self.invoke_model(&req.model, body, &cancel).await?;
        let parsed: CohereEmbedResponse = serde_json::from_slice(&bytes)
            .map_err(|e| ProviderError::Translation(format!("bedrock cohere response: {e}")))?;

        let usage = usage_from_input_tokens(header_tokens.unwrap_or(0));
        Ok(EmbedResponse {
            object: "list".to_owned(),
            data: to_embed_data(parsed.embeddings.into_vectors()),
            model: req.model,
            usage,
        })
    }
}

#[async_trait]
impl EmbeddingProvider for BedrockProvider {
    async fn embed(
        &self,
        req: EmbedRequest,
        cancel: CancellationToken,
    ) -> Result<EmbedResponse, ProviderError> {
        validate_text_input(&self.provider_name, &req)?;
        match resolve_family(&self.provider_name, &req.model)? {
            EmbedFamily::Titan => self.embed_titan(req, cancel).await,
            EmbedFamily::Cohere => self.embed_cohere(req, cancel).await,
        }
    }

    fn max_batch_size(&self) -> usize {
        MAX_BATCH_SIZE
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_core::{ContentPart, EmbedInput, EmbedItem, ImageUrl};

    #[test]
    fn resolves_titan_and_cohere_families() {
        assert_eq!(
            resolve_family("b", "amazon.titan-embed-text-v2:0").unwrap(),
            EmbedFamily::Titan
        );
        assert_eq!(
            resolve_family("b", "amazon.titan-embed-text-v1").unwrap(),
            EmbedFamily::Titan
        );
        assert_eq!(
            resolve_family("b", "us.amazon.titan-embed-text-v2:0").unwrap(),
            EmbedFamily::Titan
        );
        assert_eq!(
            resolve_family("b", "cohere.embed-english-v3").unwrap(),
            EmbedFamily::Cohere
        );
        assert_eq!(
            resolve_family("b", "cohere.embed-multilingual-v3").unwrap(),
            EmbedFamily::Cohere
        );
    }

    #[test]
    fn unknown_model_family_is_a_translation_error() {
        let err = resolve_family("b", "anthropic.claude-3-5-sonnet").unwrap_err();
        assert!(matches!(err, ProviderError::Translation(_)));
    }

    #[test]
    fn titan_request_omits_dimensions_when_absent() {
        let body = serde_json::to_value(TitanEmbedRequest {
            input_text: "hi",
            dimensions: None,
            normalize: None,
        })
        .unwrap();
        assert_eq!(body, serde_json::json!({ "inputText": "hi" }));
    }

    #[test]
    fn titan_request_carries_dimensions_and_normalize() {
        let body = serde_json::to_value(TitanEmbedRequest {
            input_text: "hi",
            dimensions: Some(512),
            normalize: Some(true),
        })
        .unwrap();
        assert_eq!(
            body,
            serde_json::json!({ "inputText": "hi", "dimensions": 512, "normalize": true })
        );
    }

    #[test]
    fn cohere_embeddings_accepts_array_and_by_type_shapes() {
        let array: CohereEmbedResponse =
            serde_json::from_value(serde_json::json!({ "embeddings": [[1.0], [2.0]] })).unwrap();
        assert_eq!(array.embeddings.into_vectors(), vec![vec![1.0], vec![2.0]]);

        let by_type: CohereEmbedResponse =
            serde_json::from_value(serde_json::json!({ "embeddings": { "float": [[3.0]] } }))
                .unwrap();
        assert_eq!(by_type.embeddings.into_vectors(), vec![vec![3.0]]);
    }

    #[test]
    fn usage_is_upstream_when_present_and_zeroed_otherwise() {
        let reported = usage_from_input_tokens(11);
        assert_eq!(reported.prompt_tokens, 11);
        assert_eq!(reported.total_tokens, 11);
        assert_eq!(reported.estimated, None);
        assert_eq!(usage_from_input_tokens(0), EmbedUsage::default());
    }

    #[test]
    fn validate_rejects_tokens_and_images_but_accepts_text() {
        let tokens = EmbedRequest {
            model: "amazon.titan-embed-text-v2:0".to_owned(),
            input: EmbedInput::Tokens(vec![1, 2]),
            encoding_format: None,
            dimensions: None,
            user: None,
            extra: serde_json::Map::new(),
        };
        assert!(validate_text_input("b", &tokens).is_err());

        let image = EmbedRequest {
            model: "cohere.embed-english-v3".to_owned(),
            input: EmbedInput::Multi(vec![EmbedItem::Parts(vec![ContentPart {
                kind: "image_url".to_owned(),
                text: None,
                image_url: Some(ImageUrl {
                    url: "data:image/png;base64,QUJD".to_owned(),
                    detail: None,
                }),
                extra: serde_json::Map::new(),
            }])]),
            encoding_format: None,
            dimensions: None,
            user: None,
            extra: serde_json::Map::new(),
        };
        assert!(matches!(
            validate_text_input("b", &image),
            Err(ProviderError::UnsupportedInput { .. })
        ));

        let text = EmbedRequest {
            model: "amazon.titan-embed-text-v2:0".to_owned(),
            input: EmbedInput::Batch(vec!["a".into(), "b".into()]),
            encoding_format: None,
            dimensions: None,
            user: None,
            extra: serde_json::Map::new(),
        };
        assert!(validate_text_input("b", &text).is_ok());
    }

    #[test]
    fn strict_mode_rejects_unsupported_dimensions() {
        assert!(matches!(
            handle_unsupported_dimensions("b", true, Some(256)),
            Err(ProviderError::UnsupportedField { .. })
        ));
        assert!(handle_unsupported_dimensions("b", false, Some(256)).is_ok());
        assert!(handle_unsupported_dimensions("b", true, None).is_ok());
    }
}
