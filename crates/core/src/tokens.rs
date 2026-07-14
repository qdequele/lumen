//! Cheap local token estimation - the ADR 003 fallback.
//!
//! When an upstream reports usage, that value wins (`estimated = false`) and
//! nothing here runs. When it doesn't (TEI reports nothing; some streams omit
//! usage), these heuristics guarantee the "never a silent zero" promise with
//! an allocation-free, hot-path-safe byte count, honestly flagged
//! `estimated = true`.
//!
//! The heuristic is the classic ~4 bytes per token for natural language and
//! JSON-ish content. It is deliberately crude: its job is honest
//! order-of-magnitude accounting, not billing-grade precision. A per-model
//! accurate tokenizer stays out of v1 (see `docs/backlog.md`).

use crate::chat::ChatRequest;
use crate::embed::EmbedRequest;
use crate::rerank::RerankRequest;

/// The heuristic's byte-per-token ratio.
const BYTES_PER_TOKEN: u64 = 4;

/// Fixed per-message overhead (role, separators) in tokens, mirroring the
/// OpenAI chat-format overhead.
const PER_MESSAGE_OVERHEAD: u64 = 4;

/// Estimate the token count of one piece of text. Non-empty text is never
/// zero tokens.
#[must_use]
pub fn estimate_text(text: &str) -> u64 {
    (text.len() as u64).div_ceil(BYTES_PER_TOKEN)
}

/// Estimate the prompt tokens of a chat request: content of every message
/// plus a fixed per-message overhead. Tool definitions and other `extra`
/// payloads are deliberately ignored - cheap and predictable beats complete.
#[must_use]
pub fn estimate_chat_prompt(req: &ChatRequest) -> u64 {
    req.messages
        .iter()
        .map(|m| PER_MESSAGE_OVERHEAD + m.content.as_ref().map_or(0, |c| estimate_text(&c.text())))
        .sum()
}

/// Estimate the input tokens of an embeddings request (sum over the batch).
#[must_use]
pub fn estimate_embed_input(req: &EmbedRequest) -> u64 {
    req.input.iter().map(estimate_text).sum()
}

/// Estimate the tokens processed by a rerank request: the query is compared
/// against every document, so it counts once per document.
#[must_use]
pub fn estimate_rerank(req: &RerankRequest) -> u64 {
    let query = estimate_text(&req.query);
    req.documents
        .iter()
        .map(|d| query + estimate_text(d.text()))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::ChatMessage;

    fn msg(content: &str) -> ChatMessage {
        ChatMessage {
            role: "user".to_owned(),
            content: Some(crate::chat::MessageContent::Text(content.to_owned())),
            name: None,
            extra: serde_json::Map::new(),
        }
    }

    #[test]
    fn text_estimate_is_never_zero_for_non_empty_input() {
        assert_eq!(estimate_text(""), 0);
        assert_eq!(estimate_text("a"), 1);
        assert_eq!(estimate_text("abcd"), 1);
        assert_eq!(estimate_text("abcde"), 2);
        // Multi-byte UTF-8 counts bytes, not chars.
        assert_eq!(estimate_text("éé"), 1); // 4 bytes
    }

    #[test]
    fn chat_prompt_counts_all_messages_plus_overhead() {
        let req = ChatRequest {
            model: "m".to_owned(),
            messages: vec![msg("12345678"), msg("")],
            temperature: None,
            top_p: None,
            max_tokens: None,
            n: None,
            stop: None,
            stream: false,
            extra: serde_json::Map::new(),
        };
        // 8 bytes → 2 tokens + 4 overhead, empty → 0 + 4 overhead.
        assert_eq!(estimate_chat_prompt(&req), 10);
    }

    #[test]
    fn embed_input_sums_the_batch() {
        let req: EmbedRequest =
            serde_json::from_str(r#"{"model":"m","input":["abcd","efghijkl"]}"#)
                .expect("valid request");
        assert_eq!(estimate_embed_input(&req), 3);
    }

    #[test]
    fn rerank_counts_query_once_per_document() {
        let req: RerankRequest =
            serde_json::from_str(r#"{"model":"m","query":"abcd","documents":["abcd","abcdefgh"]}"#)
                .expect("valid request");
        // (1 query + 1 doc) + (1 query + 2 doc) = 5
        assert_eq!(estimate_rerank(&req), 5);
    }
}
