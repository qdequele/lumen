//! Shared types, capability traits and error taxonomy for Ferrogate.
//!
//! This crate is deliberately free of any web framework, HTTP client or
//! database dependency: it defines the vocabulary the rest of the workspace
//! speaks. Three capabilities are first-class citizens — chat, embeddings and
//! reranking — each with its own request/response types and provider trait.
//!
//! # Modules
//! * [`chat`] — OpenAI `chat/completions` request/response/chunk types.
//! * [`embed`] — OpenAI `embeddings` types.
//! * [`rerank`] — Cohere `rerank` types.
//! * [`provider`] — the [`ChatProvider`], [`EmbeddingProvider`] and
//!   [`RerankProvider`] traits.
//! * [`error`] — the [`ProviderError`] / [`GatewayError`] taxonomy.
//! * [`capability`] — the [`Capability`] enum.

#![forbid(unsafe_code)]

pub mod capability;
pub mod chat;
pub mod embed;
pub mod error;
pub mod provider;
pub mod rerank;

pub use capability::Capability;
pub use chat::{
    ChatChoice, ChatChunk, ChatChunkChoice, ChatDelta, ChatMessage, ChatRequest, ChatResponse,
    Usage,
};
pub use embed::{EmbedData, EmbedInput, EmbedRequest, EmbedResponse, EmbedUsage};
pub use error::{ErrorBody, ErrorEnvelope, ErrorType, GatewayError, ProviderError};
pub use provider::{ChatProvider, EmbeddingProvider, RerankProvider};
pub use rerank::{RerankDocument, RerankRequest, RerankResponse, RerankResult};

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
    fn rerank_request_parses_cohere_shape() {
        let req: RerankRequest = serde_json::from_str(
            r#"{"model":"rerank-v3","query":"q","documents":["d1","d2"],"top_n":1}"#,
        )
        .unwrap();
        assert_eq!(req.query, "q");
        assert_eq!(req.documents.len(), 2);
        assert_eq!(req.top_n, Some(1));
    }
}
