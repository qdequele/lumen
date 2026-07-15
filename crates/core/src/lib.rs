//! Shared types, capability traits and error taxonomy for LUMEN.
//!
//! This crate is deliberately free of any web framework, HTTP client or
//! database dependency: it defines the vocabulary the rest of the workspace
//! speaks. Three capabilities are first-class citizens - chat, embeddings and
//! reranking - each with its own request/response types and provider trait.
//!
//! # Modules
//! * [`chat`] - OpenAI `chat/completions` request/response/chunk types.
//! * [`embed`] - OpenAI `embeddings` types.
//! * [`rerank`] - Cohere `rerank` types.
//! * [`provider`] - the [`ChatProvider`], [`EmbeddingProvider`] and
//!   [`RerankProvider`] traits.
//! * [`error`] - the [`ProviderError`] / [`GatewayError`] taxonomy.
//! * [`capability`] - the [`Capability`] enum.

#![forbid(unsafe_code)]

pub mod capability;
pub mod chat;
pub mod embed;
pub mod error;
pub mod media;
pub mod provider;
pub mod rerank;
pub mod tokens;

pub use capability::Capability;
pub use chat::{
    ChatChoice, ChatChunk, ChatChunkChoice, ChatDelta, ChatMessage, ChatRequest, ChatResponse,
    ContentPart, DataUri, ImageUrl, MessageContent, Usage,
};
pub use embed::{
    encode_embedding_base64, EmbedData, EmbedInput, EmbedItem, EmbedRequest, EmbedResponse,
    EmbedUsage, EmbeddingEncoding,
};
pub use error::{ErrorBody, ErrorEnvelope, ErrorType, GatewayError, ProviderError, QuotaKind};
pub use media::{measure_media, MediaTypeUsage, MediaUsage};
pub use provider::{ChatProvider, EmbeddingProvider, RerankProvider};
pub use rerank::{
    RerankDocument, RerankRequest, RerankResponse, RerankResult, RerankResultDocument, RerankUsage,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_request_preserves_unknown_fields_for_passthrough() {
        let raw = r#"{
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "hi"}],
            "temperature": 0.7,
            "tools": [{"type": "function"}],
            "custom_provider_flag": true
        }"#;
        let req: ChatRequest = serde_json::from_str(raw).unwrap();
        assert_eq!(req.model, "gpt-4o");
        assert_eq!(req.temperature, Some(0.7));
        assert!(!req.stream);
        // Unknown fields survive round-trip via `extra`.
        assert!(req.extra.contains_key("tools"));
        assert!(req.extra.contains_key("custom_provider_flag"));
        let back = serde_json::to_value(&req).unwrap();
        assert_eq!(back["custom_provider_flag"], true);
    }

    #[test]
    fn embed_data_decodes_base64_embedding_to_floats() {
        use base64::Engine;
        // base64 of the little-endian bytes of [1.0f32, 2.0f32].
        let bytes: Vec<u8> = [1.0f32, 2.0f32]
            .iter()
            .flat_map(|f| f.to_le_bytes())
            .collect();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let json = format!(r#"{{"object":"embedding","index":0,"embedding":"{b64}"}}"#);
        let data: EmbedData = serde_json::from_str(&json).unwrap();
        assert_eq!(data.embedding, vec![1.0, 2.0]);
    }

    #[test]
    fn embed_data_still_accepts_float_array() {
        let json = r#"{"object":"embedding","index":1,"embedding":[0.5,-0.5]}"#;
        let data: EmbedData = serde_json::from_str(json).unwrap();
        assert_eq!(data.embedding, vec![0.5, -0.5]);
    }

    #[test]
    fn embed_input_accepts_single_or_batch() {
        let single: EmbedRequest = serde_json::from_str(r#"{"model":"m","input":"one"}"#).unwrap();
        assert_eq!(single.input.len(), 1);

        let batch: EmbedRequest =
            serde_json::from_str(r#"{"model":"m","input":["a","b","c"]}"#).unwrap();
        assert_eq!(batch.input.len(), 3);
        let texts: Vec<&str> = batch.input.iter().collect();
        assert_eq!(texts, vec!["a", "b", "c"]);
    }

    #[test]
    fn embed_input_accepts_multimodal_parts() {
        let raw = r#"{
            "model": "m",
            "input": [
                "plain text item",
                [
                    {"type": "text", "text": "a caption"},
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}
                ]
            ]
        }"#;
        let req: EmbedRequest = serde_json::from_str(raw).unwrap();
        assert_eq!(req.input.len(), 2);
        assert!(req.input.has_image());
        // Round-trips back to an equivalent JSON structure.
        let back = serde_json::to_value(&req).unwrap();
        assert_eq!(back["input"][0], "plain text item");
        assert_eq!(
            back["input"][1][1]["image_url"]["url"],
            "data:image/png;base64,AAAA"
        );
    }

    #[test]
    fn content_part_type_defaults_to_text() {
        // No `"type"` → defaults to text.
        let raw = r#"{"model":"m","input":[[{"text":"hi"}]]}"#;
        let req: EmbedRequest = serde_json::from_str(raw).unwrap();
        assert!(!req.input.has_image());
        let texts: Vec<&str> = req.input.text_iter().collect();
        assert_eq!(texts, vec!["hi"]);
    }

    #[test]
    fn untyped_image_url_part_is_detected_as_image() {
        // No `"type"` but an `image_url` field → dispatch by field presence.
        let raw = r#"{"model":"m","input":[[{"image_url":{"url":"data:image/png;base64,AA"}}]]}"#;
        let req: EmbedRequest = serde_json::from_str(raw).unwrap();
        assert!(req.input.has_image());
    }

    #[test]
    fn unknown_part_type_survives_round_trip() {
        let raw = r#"{"model":"m","input":[[{"type":"input_audio","input_audio":{"data":"x"}}]]}"#;
        let req: EmbedRequest = serde_json::from_str(raw).unwrap();
        assert!(!req.input.has_image());
        let back = serde_json::to_value(&req).unwrap();
        assert_eq!(back["input"][0][0]["type"], "input_audio");
        assert_eq!(back["input"][0][0]["input_audio"]["data"], "x");
    }

    #[test]
    fn text_only_array_parses_as_batch_not_multi() {
        // Untagged order: an all-strings array is `Batch`, never `Multi`.
        let req: EmbedInput = serde_json::from_str(r#"["a","b"]"#).unwrap();
        assert!(matches!(req, EmbedInput::Batch(_)));
        // A bare string is `Single`.
        let single: EmbedInput = serde_json::from_str(r#""x""#).unwrap();
        assert!(matches!(single, EmbedInput::Single(_)));
    }

    #[test]
    fn text_iter_gathers_all_text_fragments() {
        let raw = r#"{
            "model":"m",
            "input":[
                "one",
                [{"type":"text","text":"two"},{"type":"image_url","image_url":{"url":"data:image/png;base64,AA"}},{"type":"text","text":"three"}]
            ]
        }"#;
        let req: EmbedRequest = serde_json::from_str(raw).unwrap();
        let texts: Vec<&str> = req.input.text_iter().collect();
        assert_eq!(texts, vec!["one", "two", "three"]);
    }

    #[test]
    fn embed_request_captures_input_type_via_extra_without_reserializing_it() {
        // A caller-supplied `input_type` (Cohere's search_query vs
        // search_document override, issue #22) is captured into `extra` for
        // provider translation code, but is NOT re-serialized: the
        // OpenAI-compatible passthrough providers send the serialized request
        // as the outgoing body and unknown fields must stop at the gateway.
        let raw = r#"{"model":"m","input":"hello","input_type":"search_query"}"#;
        let req: EmbedRequest = serde_json::from_str(raw).unwrap();
        assert_eq!(
            req.extra.get("input_type").and_then(|v| v.as_str()),
            Some("search_query")
        );
        let back = serde_json::to_value(&req).unwrap();
        assert!(
            back.get("input_type").is_none(),
            "extra fields must not re-serialize, got: {back}"
        );
        assert_eq!(back["model"], "m");
        assert_eq!(back["input"], "hello");
    }

    #[test]
    fn rerank_request_parses_cohere_shape() {
        let req: RerankRequest = serde_json::from_str(
            r#"{"model":"rerank-v3","query":"q","documents":["d1","d2"],"top_n":1}"#,
        )
        .unwrap();
        assert_eq!(req.query, "q");
        assert_eq!(req.documents.len(), 2);
        assert_eq!(req.top_n, Some(1));
        // Absent `return_documents` defaults to false (bandwidth-saving).
        assert!(!req.return_documents);
    }

    #[test]
    fn rerank_documents_accept_strings_and_objects() {
        let req: RerankRequest = serde_json::from_str(
            r#"{"model":"m","query":"q","documents":["bare",{"text":"wrapped"}],"return_documents":true}"#,
        )
        .unwrap();
        assert_eq!(req.documents.len(), 2);
        // Both forms expose their text uniformly.
        assert_eq!(req.documents[0].text(), "bare");
        assert_eq!(req.documents[1].text(), "wrapped");
        assert!(req.return_documents);
    }

    #[test]
    fn rerank_response_omits_document_when_absent() {
        let resp = RerankResponse {
            results: vec![RerankResult {
                index: 0,
                relevance_score: 0.9,
                document: None,
            }],
            usage: RerankUsage {
                search_units: 1,
                estimated: None,
                ..Default::default()
            },
        };
        let json = serde_json::to_value(&resp).unwrap();
        // `document` is skipped entirely when None (criterion 5).
        assert!(json["results"][0].get("document").is_none());
        assert_eq!(json["usage"]["search_units"], 1);
    }
}
