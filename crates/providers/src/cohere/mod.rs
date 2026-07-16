//! Cohere provider (API v2) - chat (Command R / R+), embeddings and
//! reranking.
//!
//! Cohere's wire schema differs from the internal (OpenAI/Cohere-inspired)
//! types in both directions, so this module translates:
//!
//! * chat: `POST /v2/chat` - request/response translation lives in [`chat`],
//!   SSE streaming-event translation in [`stream`] (see their module docs -
//!   the wire shape is OpenAI-adjacent, closer than Anthropic's);
//! * embed: `POST /v2/embed` takes `{ model, texts, input_type, embedding_types }`
//!   and returns `{ embeddings: { float: [[..]] }, meta: { billed_units } }`.
//!   `input_type` defaults to `search_document` but a caller may override it
//!   (e.g. `search_query`) via the `input_type` field on an otherwise
//!   OpenAI-shaped `/v1/embeddings` request (issue #22) - see
//!   [`ALLOWED_INPUT_TYPES`] and [`resolve_input_type`];
//! * rerank: `POST /v2/rerank` takes `{ model, query, documents, top_n }` and
//!   returns `{ results: [{ index, relevance_score }], meta: { billed_units } }`.
//!
//! The gateway (`crate::rerank`) owns ordering, `top_n` clamping and document
//! echoing, so the rerank translation only carries indices, scores and usage.

mod chat;
mod stream;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use lumen_core::{
    ChatChunk, ChatProvider, ChatRequest, ChatResponse, ContentPart, EmbedData, EmbedInput,
    EmbedItem, EmbedRequest, EmbedResponse, EmbedUsage, EmbeddingProvider, ProviderError,
    RerankProvider, RerankRequest, RerankResponse, RerankResult, RerankUsage,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;
use tokio_util::sync::CancellationToken;

use self::stream::CohereTranslator;
use crate::chat::{items_to_chunks, items_to_sse_bytes, translate_sse_stream, StreamItem};
use crate::http::{open_stream, post_json};

/// Default Cohere API base (no version suffix; paths add `/v2/...`).
const DEFAULT_BASE_URL: &str = "https://api.cohere.com";

/// Cohere's documented maximum number of texts per embed request.
const MAX_BATCH_SIZE: usize = 96;

/// OpenAI chat fields the v2 chat translation cannot honor (issue #72):
/// `logprobs` exists upstream but its response shape is not translated back
/// to OpenAI's (`top_logprobs` rides the same response shape), and
/// `logit_bias` / `parallel_tool_calls` have no v2 equivalent. Rejected
/// (strict) or dropped with a trace (lenient) before any upstream call.
/// `response_format` and `seed` are NOT here: they map natively in
/// [`chat::translate_request`](self::chat).
const UNSUPPORTED_CHAT_FIELDS: &[&str] = &[
    "logprobs",
    "top_logprobs",
    "logit_bias",
    "parallel_tool_calls",
];

/// A Cohere provider serving embeddings and reranking.
pub struct CohereProvider {
    client: reqwest::Client,
    provider_name: String,
    base_url: String,
    /// Bearer token. Redacted from `Debug`; never logged.
    api_key: Option<String>,
    /// When `true`, reject a chat request that sets an OpenAI field the v2
    /// translation cannot honor ([`UNSUPPORTED_CHAT_FIELDS`]) with a 400
    /// (`LM-1001`) instead of silently dropping it (issue #72).
    strict: bool,
}

impl CohereProvider {
    /// Construct a provider. `base_url` defaults to the public Cohere API.
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
            strict: false,
        }
    }

    /// Set strict mode: reject (400, `LM-1001`) rather than drop chat request
    /// fields the v2 translation cannot honor (issue #72). Defaults to `false`
    /// (lenient: drop with a `debug!` trace).
    #[must_use]
    pub fn with_strict(mut self, strict: bool) -> Self {
        self.strict = strict;
        self
    }
}

/// Redacted so the API key can never reach a log line via `{:?}`.
impl fmt::Debug for CohereProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CohereProvider")
            .field("provider_name", &self.provider_name)
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .finish_non_exhaustive()
    }
}

// ---- Chat ------------------------------------------------------------------

impl CohereProvider {
    /// Open the upstream stream and translate its events (shared by both
    /// streaming trait methods).
    async fn open_translated_stream(
        &self,
        req: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, Result<StreamItem, ProviderError>>, ProviderError> {
        crate::mapping::check_unsupported_chat_fields(
            &self.provider_name,
            self.strict,
            &req.extra,
            UNSUPPORTED_CHAT_FIELDS,
        )?;
        let url = format!("{}/v2/chat", self.base_url);
        let body = chat::translate_request(&req, true);

        let bytes = open_stream(
            &self.client,
            &url,
            &body,
            self.api_key.as_deref(),
            &self.provider_name,
            &cancel,
        )
        .await?;
        Ok(translate_sse_stream(
            bytes,
            CohereTranslator::new(&req.model),
        ))
    }
}

#[async_trait]
impl ChatProvider for CohereProvider {
    async fn chat(
        &self,
        req: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<ChatResponse, ProviderError> {
        crate::mapping::check_unsupported_chat_fields(
            &self.provider_name,
            self.strict,
            &req.extra,
            UNSUPPORTED_CHAT_FIELDS,
        )?;
        let url = format!("{}/v2/chat", self.base_url);
        let body = chat::translate_request(&req, false);

        let bytes = post_json(
            &self.client,
            &url,
            &body,
            self.api_key.as_deref(),
            &self.provider_name,
            &cancel,
        )
        .await?;

        let parsed: chat::CohereChatResponse = serde_json::from_slice(&bytes)
            .map_err(|e| ProviderError::Translation(format!("cohere chat response: {e}")))?;
        Ok(chat::translate_response(parsed, &req.model))
    }

    async fn chat_stream(
        &self,
        req: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, Result<ChatChunk, ProviderError>>, ProviderError> {
        let items = self.open_translated_stream(req, cancel).await?;
        Ok(items_to_chunks(items))
    }

    /// Event-by-event translation to OpenAI SSE frames. `data: [DONE]` is
    /// emitted only on a genuine upstream `message-end`, so a mid-stream
    /// upstream death surfaces as a missing terminator (LM-3010 downstream).
    async fn chat_stream_bytes(
        &self,
        req: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, Result<Bytes, ProviderError>>, ProviderError> {
        let items = self.open_translated_stream(req, cancel).await?;
        Ok(items_to_sse_bytes(items))
    }
}

// ---- Embeddings ----------------------------------------------------------

/// Either the text-only `texts` shape or the multimodal `inputs` content-array
/// shape (embed-v4). Untagged so each variant serializes as its bare fields.
#[derive(Serialize)]
#[serde(untagged)]
enum CohereEmbedBody<'a> {
    Text(CohereEmbedRequest<'a>),
    Multi(CohereEmbedMultiRequest<'a>),
}

#[derive(Serialize)]
struct CohereEmbedRequest<'a> {
    model: &'a str,
    texts: Vec<&'a str>,
    /// Required by v2. Defaults to `search_document` (the indexing case) but
    /// honors a caller's override (issue #22) - see [`resolve_input_type`].
    input_type: &'a str,
    embedding_types: [&'static str; 1],
}

/// Multimodal request body (embed-v4): each batch item becomes one `inputs`
/// entry with an ordered `content` array of text/image parts.
#[derive(Serialize)]
struct CohereEmbedMultiRequest<'a> {
    model: &'a str,
    inputs: Vec<CohereInput<'a>>,
    input_type: &'a str,
    embedding_types: [&'static str; 1],
}

#[derive(Serialize)]
struct CohereInput<'a> {
    content: Vec<CohereContent<'a>>,
}

/// One content part in Cohere's embed-v4 shape:
/// `{"type":"text","text":...}` or `{"type":"image_url","image_url":{"url":...}}`.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum CohereContent<'a> {
    Text { text: &'a str },
    ImageUrl { image_url: CohereImageUrl<'a> },
}

#[derive(Serialize)]
struct CohereImageUrl<'a> {
    url: &'a str,
}

/// Cohere's `input_type` for the indexing case (the gateway does not know
/// query-vs-document intent unless the caller says so - see
/// [`resolve_input_type`]).
const DEFAULT_INPUT_TYPE: &str = "search_document";

/// Cohere embed v2's accepted `input_type` values. A request `input_type`
/// outside this set is rejected at the gateway edge with `LM-1001` before any
/// upstream call is made (`crates/server/src/embeddings.rs`), so any value
/// reaching [`resolve_input_type`] is already trusted.
pub const ALLOWED_INPUT_TYPES: [&str; 4] = [
    "search_document",
    "search_query",
    "classification",
    "clustering",
];

/// Resolve the effective `input_type`: the caller's override carried in
/// `EmbedRequest::extra` (issue #22 - a caller sets `"input_type":
/// "search_query"` on an otherwise OpenAI-shaped `/v1/embeddings` request), or
/// the `search_document` indexing default when absent.
fn resolve_input_type(req: &EmbedRequest) -> &str {
    req.extra
        .get("input_type")
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_INPUT_TYPE)
}

/// Build the request body, choosing the text `texts` shape or the multimodal
/// `inputs` content-array shape (used whenever the input is a content-parts
/// batch, so per-item grouping and images are preserved).
fn build_cohere_body(req: &EmbedRequest) -> CohereEmbedBody<'_> {
    let input_type = resolve_input_type(req);
    match &req.input {
        EmbedInput::Multi(items) => {
            let inputs = items
                .iter()
                .map(|item| {
                    let content = match item {
                        EmbedItem::Text(s) => vec![CohereContent::Text { text: s }],
                        EmbedItem::Parts(parts) => parts.iter().map(part_to_content).collect(),
                    };
                    CohereInput { content }
                })
                .collect();
            CohereEmbedBody::Multi(CohereEmbedMultiRequest {
                model: &req.model,
                inputs,
                input_type,
                embedding_types: ["float"],
            })
        }
        // Token-array inputs never reach here: `embed()` rejects them up front
        // (reject_pretokenized_input, issue #25). The arms stay total so a
        // future call site cannot silently send an empty texts array.
        EmbedInput::Single(_)
        | EmbedInput::Batch(_)
        | EmbedInput::Tokens(_)
        | EmbedInput::TokenBatch(_) => CohereEmbedBody::Text(CohereEmbedRequest {
            model: &req.model,
            texts: req.input.iter().collect(),
            input_type,
            embedding_types: ["float"],
        }),
    }
}

/// Translate one content part to Cohere's content shape (dispatch by field
/// presence: an `image_url` is an image regardless of its declared `type`).
fn part_to_content(part: &ContentPart) -> CohereContent<'_> {
    if let Some(image) = part.image() {
        CohereContent::ImageUrl {
            image_url: CohereImageUrl { url: &image.url },
        }
    } else {
        CohereContent::Text {
            text: part.text_str().unwrap_or(""),
        }
    }
}

#[derive(Deserialize)]
struct CohereEmbedResponse {
    embeddings: CohereEmbeddings,
    #[serde(default)]
    meta: CohereMeta,
}

#[derive(Deserialize)]
struct CohereEmbeddings {
    #[serde(default)]
    float: Vec<Vec<f32>>,
}

#[derive(Default, Deserialize)]
struct CohereMeta {
    #[serde(default)]
    billed_units: CohereBilledUnits,
}

#[derive(Default, Deserialize)]
struct CohereBilledUnits {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    search_units: u32,
}

#[async_trait]
impl EmbeddingProvider for CohereProvider {
    async fn embed(
        &self,
        req: EmbedRequest,
        cancel: CancellationToken,
    ) -> Result<EmbedResponse, ProviderError> {
        // Cohere embed takes texts only; token-id arrays would serialize to an
        // EMPTY texts array. Honest 400 before any upstream call (issue #25).
        crate::mapping::reject_pretokenized_input(&self.provider_name, &req.input)?;
        let url = format!("{}/v2/embed", self.base_url);
        let body = build_cohere_body(&req);

        let bytes = post_json(
            &self.client,
            &url,
            &body,
            self.api_key.as_deref(),
            &self.provider_name,
            &cancel,
        )
        .await?;

        let parsed: CohereEmbedResponse = serde_json::from_slice(&bytes)
            .map_err(|e| ProviderError::Translation(format!("cohere embed response: {e}")))?;

        let data = parsed
            .embeddings
            .float
            .into_iter()
            .enumerate()
            .map(|(index, embedding)| EmbedData {
                object: "embedding".to_owned(),
                index: u32::try_from(index).unwrap_or(u32::MAX),
                embedding,
                encoding: lumen_core::EmbeddingEncoding::default(),
            })
            .collect();

        Ok(EmbedResponse {
            object: "list".to_owned(),
            data,
            model: req.model,
            usage: EmbedUsage {
                prompt_tokens: parsed.meta.billed_units.input_tokens,
                total_tokens: parsed.meta.billed_units.input_tokens,
                estimated: None,
            },
        })
    }

    fn max_batch_size(&self) -> usize {
        MAX_BATCH_SIZE
    }
}

// ---- Reranking -----------------------------------------------------------

#[derive(Serialize)]
struct CohereRerankRequest<'a> {
    model: &'a str,
    query: &'a str,
    documents: Vec<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_n: Option<u32>,
}

#[derive(Deserialize)]
struct CohereRerankResponse {
    #[serde(default)]
    results: Vec<CohereRerankResult>,
    #[serde(default)]
    meta: CohereMeta,
}

#[derive(Deserialize)]
struct CohereRerankResult {
    index: u32,
    relevance_score: f32,
}

#[async_trait]
impl RerankProvider for CohereProvider {
    async fn rerank(
        &self,
        req: RerankRequest,
        cancel: CancellationToken,
    ) -> Result<RerankResponse, ProviderError> {
        let url = format!("{}/v2/rerank", self.base_url);
        let documents: Vec<&str> = req
            .documents
            .iter()
            .map(lumen_core::RerankDocument::text)
            .collect();
        let body = CohereRerankRequest {
            model: &req.model,
            query: &req.query,
            documents,
            top_n: req.top_n,
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

        let parsed: CohereRerankResponse = serde_json::from_slice(&bytes)
            .map_err(|e| ProviderError::Translation(format!("cohere rerank response: {e}")))?;

        Ok(RerankResponse {
            results: parsed
                .results
                .into_iter()
                .map(|r| RerankResult {
                    index: r.index,
                    relevance_score: r.relevance_score,
                    document: None,
                })
                .collect(),
            usage: RerankUsage {
                search_units: parsed.meta.billed_units.search_units,
                estimated: None,
                // Cohere does not report a token count; the gateway derives
                // one for uniform observability (ADR 003), see
                // `lumen_server::rerank`.
                ..Default::default()
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The API key must never reach a log line via `{:?}` (CLAUDE.md rule 5).
    #[test]
    fn debug_format_never_leaks_the_api_key() {
        let provider = CohereProvider::new(
            reqwest::Client::new(),
            "cohere-test",
            None,
            Some("sk-super-secret-value".to_owned()),
        );
        let debugged = format!("{provider:?}");
        assert!(!debugged.contains("sk-super-secret-value"));
        assert!(debugged.contains("redacted"));
    }
}
