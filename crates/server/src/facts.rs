//! Per-request [`FactSource`] for virtual-model `switch` conditions
//! (ADR 014). Built before routing, dropped before any `.await`; the
//! metadata header and the token estimate are computed only when a
//! condition asks for them.

use std::cell::OnceCell;

use axum::http::HeaderMap;
use lumen_core::{tokens, ChatRequest, EmbedRequest, MessageContent, RerankRequest};
use lumen_router::virtual_models::FactSource;
use serde_json::Value;

use crate::auth::AuthedKey;
use crate::metadata::{MetadataOutcome, RequestMetadata};

/// The routing-relevant facts of one request.
pub struct Facts<'a> {
    group: Option<String>,
    headers: &'a HeaderMap,
    metadata: OnceCell<Option<RequestMetadata>>,
    has_images: bool,
    has_tools: bool,
    stream: bool,
    documents: Option<u64>,
    input_tokens: OnceCell<u64>,
    estimate: Box<dyn Fn() -> u64 + 'a>,
}

impl<'a> Facts<'a> {
    /// Facts with the key's group, the headers and the lazy token estimate; the
    /// per-capability constructors fill in the rest.
    fn new(
        headers: &'a HeaderMap,
        key: Option<&AuthedKey>,
        estimate: Box<dyn Fn() -> u64 + 'a>,
    ) -> Self {
        Self {
            group: key.and_then(|AuthedKey(entry)| entry.group_id()),
            headers,
            metadata: OnceCell::new(),
            has_images: false,
            has_tools: false,
            stream: false,
            documents: None,
            input_tokens: OnceCell::new(),
            estimate,
        }
    }

    /// Facts of a chat request.
    #[must_use]
    pub fn chat(headers: &'a HeaderMap, key: Option<&AuthedKey>, req: &'a ChatRequest) -> Self {
        let mut facts = Self::new(
            headers,
            key,
            Box::new(move || tokens::estimate_chat_prompt(req)),
        );
        facts.has_images = req
            .messages
            .iter()
            .any(|m| m.content.as_ref().is_some_and(MessageContent::has_image));
        facts.has_tools = req
            .extra
            .get("tools")
            .and_then(Value::as_array)
            .is_some_and(|tools| !tools.is_empty());
        facts.stream = req.stream;
        facts
    }

    /// Facts of an embeddings request.
    #[must_use]
    pub fn embed(headers: &'a HeaderMap, key: Option<&AuthedKey>, req: &'a EmbedRequest) -> Self {
        let mut facts = Self::new(
            headers,
            key,
            Box::new(move || tokens::estimate_embed_input(req)),
        );
        facts.has_images = req.input.has_image();
        facts
    }

    /// Facts of a rerank request.
    #[must_use]
    pub fn rerank(headers: &'a HeaderMap, key: Option<&AuthedKey>, req: &'a RerankRequest) -> Self {
        let mut facts = Self::new(headers, key, Box::new(move || tokens::estimate_rerank(req)));
        facts.documents = u64::try_from(req.documents.len()).ok();
        facts
    }

    /// Facts of a `SystemOne` request (group and metadata only).
    #[must_use]
    pub fn systemone(headers: &'a HeaderMap, key: Option<&AuthedKey>) -> Self {
        Self::new(headers, key, Box::new(|| 0))
    }
}

impl FactSource for Facts<'_> {
    fn group(&self) -> Option<&str> {
        self.group.as_deref()
    }

    fn metadata(&self, key: &str) -> Option<&Value> {
        self.metadata
            .get_or_init(|| match RequestMetadata::extract(self.headers) {
                MetadataOutcome::Valid(meta) => Some(meta),
                MetadataOutcome::Absent | MetadataOutcome::Rejected(_) => None,
            })
            .as_ref()?
            .get(key)
    }

    fn has_images(&self) -> bool {
        self.has_images
    }

    fn has_tools(&self) -> bool {
        self.has_tools
    }

    fn stream(&self) -> bool {
        self.stream
    }

    fn input_tokens(&self) -> u64 {
        *self.input_tokens.get_or_init(|| (self.estimate)())
    }

    fn documents(&self) -> Option<u64> {
        self.documents
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn chat_facts_read_tools_stream_and_metadata_lazily() {
        let req: ChatRequest = serde_json::from_value(json!({
            "model": "m", "stream": true,
            "messages": [{ "role": "user", "content": "hello" }],
            "tools": [{ "type": "function", "function": { "name": "f" } }]
        }))
        .unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("x-lumen-metadata", r#"{"plan":"pro"}"#.parse().unwrap());
        let facts = Facts::chat(&headers, None, &req);
        assert!(facts.has_tools() && facts.stream() && !facts.has_images());
        assert_eq!(facts.group(), None);
        assert!(
            facts.metadata.get().is_none(),
            "metadata is parsed only on demand"
        );
        assert_eq!(facts.metadata("plan"), Some(&json!("pro")));
        assert!(facts.input_tokens() > 0);
    }

    #[test]
    fn rerank_facts_count_documents() {
        let req: RerankRequest =
            serde_json::from_value(json!({ "model": "m", "query": "q", "documents": ["a", "b"] }))
                .unwrap();
        let headers = HeaderMap::new();
        assert_eq!(Facts::rerank(&headers, None, &req).documents(), Some(2));
    }
}
