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
//!
//! Image parts get their own flat per-image estimate (see
//! [`estimate_image_tokens`]) rather than `0`, per the ADR 003 vision
//! addendum: the gateway never decodes image bytes on the request path, so a
//! true per-dimension tile count is out of reach here and stays a backlog
//! item.

use crate::chat::{ChatRequest, MessageContent};
use crate::embed::EmbedRequest;
use crate::rerank::RerankRequest;

/// The heuristic's byte-per-token ratio.
const BYTES_PER_TOKEN: u64 = 4;

/// Fixed per-message overhead (role, separators) in tokens, mirroring the
/// OpenAI chat-format overhead.
const PER_MESSAGE_OVERHEAD: u64 = 4;

/// Flat token cost for an image part sent with `"detail": "low"`. OpenAI's
/// low-detail vision cost is a fixed 85 tokens regardless of resolution, so
/// this value is exact rather than an approximation - no pixel dimensions
/// are needed to reproduce it.
const LOW_DETAIL_IMAGE_TOKENS: u64 = 85;

/// Flat token cost for an image part sent with `"detail": "high"`/`"auto"`,
/// or with no `detail` at all (OpenAI's own default).
///
/// OpenAI's real high-detail formula is `85 + 170 * tiles`, where `tiles` is
/// derived from the image's decoded pixel dimensions (scaled to fit
/// 2048x2048, then to 768px on the shortest side, then tiled in 512x512
/// blocks). This gateway does not decode image bytes on the request path
/// (ADR 003, hot-path rule), so the real dimensions are never available here
/// and a true per-image tile count stays a backlog item (`docs/backlog.md`).
///
/// This constant substitutes a single flat estimate for a mid-size ~1024x1024
/// photo (scales to 768x768 -> a 2x2 grid of tiles: `85 + 4 * 170 = 765`),
/// so a no-usage vision request is no longer silently under-counted to `0`.
/// It is deliberately biased toward "closer to typical" rather than exact.
const DEFAULT_IMAGE_TOKENS: u64 = 765;

/// Estimate the token cost of one image part from its `detail` hint, per the
/// flat per-image heuristic documented on [`LOW_DETAIL_IMAGE_TOKENS`] and
/// [`DEFAULT_IMAGE_TOKENS`]. Never inspects the image bytes themselves.
#[must_use]
pub fn estimate_image_tokens(detail: Option<&str>) -> u64 {
    match detail {
        Some("low") => LOW_DETAIL_IMAGE_TOKENS,
        _ => DEFAULT_IMAGE_TOKENS,
    }
}

/// Sum the per-image token estimate over every image part of one message's
/// content. Text parts and non-image `extra` payloads contribute nothing
/// here (see [`estimate_text`] for the text side).
fn estimate_content_images(content: &MessageContent) -> u64 {
    match content {
        MessageContent::Text(_) => 0,
        MessageContent::Parts(parts) => parts
            .iter()
            .filter_map(crate::chat::ContentPart::image)
            .map(|img| estimate_image_tokens(img.detail.as_deref()))
            .sum(),
    }
}

/// Estimate the token count of one piece of text. Non-empty text is never
/// zero tokens.
#[must_use]
pub fn estimate_text(text: &str) -> u64 {
    (text.len() as u64).div_ceil(BYTES_PER_TOKEN)
}

/// Estimate the prompt tokens of a chat request: content of every message
/// (text plus the flat per-image estimate for any image parts) plus a fixed
/// per-message overhead. Tool definitions and other `extra` payloads are
/// deliberately ignored - cheap and predictable beats complete.
#[must_use]
pub fn estimate_chat_prompt(req: &ChatRequest) -> u64 {
    req.messages
        .iter()
        .map(|m| {
            PER_MESSAGE_OVERHEAD
                + m.content
                    .as_ref()
                    .map_or(0, |c| estimate_text(&c.text()) + estimate_content_images(c))
        })
        .sum()
}

/// Estimate the input tokens of an embeddings request (sum over the batch).
/// Pre-tokenized inputs (token-id arrays) contribute one token per id, with no
/// byte heuristic; text inputs use the byte-per-token heuristic.
#[must_use]
pub fn estimate_embed_input(req: &EmbedRequest) -> u64 {
    let text: u64 = req.input.iter().map(estimate_text).sum();
    text.saturating_add(req.input.token_count())
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
    use crate::chat::{ChatMessage, ContentPart, ImageUrl, MessageContent};

    fn msg(content: &str) -> ChatMessage {
        ChatMessage {
            role: "user".to_owned(),
            content: Some(crate::chat::MessageContent::Text(content.to_owned())),
            name: None,
            extra: serde_json::Map::new(),
        }
    }

    fn text_part(text: &str) -> ContentPart {
        ContentPart {
            kind: "text".to_owned(),
            text: Some(text.to_owned()),
            image_url: None,
            extra: serde_json::Map::new(),
        }
    }

    fn image_part(detail: Option<&str>) -> ContentPart {
        ContentPart {
            kind: "image_url".to_owned(),
            text: None,
            image_url: Some(ImageUrl {
                url: "data:image/png;base64,AAAA".to_owned(),
                detail: detail.map(str::to_owned),
            }),
            extra: serde_json::Map::new(),
        }
    }

    fn parts_msg(parts: Vec<ContentPart>) -> ChatMessage {
        ChatMessage {
            role: "user".to_owned(),
            content: Some(MessageContent::Parts(parts)),
            name: None,
            extra: serde_json::Map::new(),
        }
    }

    fn request(messages: Vec<ChatMessage>) -> ChatRequest {
        ChatRequest {
            model: "m".to_owned(),
            messages,
            temperature: None,
            top_p: None,
            max_tokens: None,
            n: None,
            stop: None,
            stream: false,
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
    fn embed_token_arrays_count_one_per_id() {
        let single: EmbedRequest =
            serde_json::from_str(r#"{"model":"m","input":[1,2,3,4]}"#).expect("valid request");
        assert_eq!(estimate_embed_input(&single), 4);

        let batch: EmbedRequest = serde_json::from_str(r#"{"model":"m","input":[[1,2],[3,4,5]]}"#)
            .expect("valid request");
        assert_eq!(estimate_embed_input(&batch), 5);
    }

    #[test]
    fn rerank_counts_query_once_per_document() {
        let req: RerankRequest =
            serde_json::from_str(r#"{"model":"m","query":"abcd","documents":["abcd","abcdefgh"]}"#)
                .expect("valid request");
        // (1 query + 1 doc) + (1 query + 2 doc) = 5
        assert_eq!(estimate_rerank(&req), 5);
    }

    #[test]
    fn image_part_adds_strictly_more_tokens_than_text_only() {
        let text_only = request(vec![parts_msg(vec![text_part("hello")])]);
        let with_image = request(vec![parts_msg(vec![text_part("hello"), image_part(None)])]);
        assert!(estimate_chat_prompt(&with_image) > estimate_chat_prompt(&text_only));
        assert_eq!(
            estimate_chat_prompt(&with_image) - estimate_chat_prompt(&text_only),
            DEFAULT_IMAGE_TOKENS
        );
    }

    #[test]
    fn image_only_message_is_no_longer_undercounted_to_overhead_alone() {
        let req = request(vec![parts_msg(vec![image_part(None)])]);
        assert_eq!(
            estimate_chat_prompt(&req),
            PER_MESSAGE_OVERHEAD + DEFAULT_IMAGE_TOKENS
        );
    }

    #[test]
    fn low_detail_image_uses_the_flat_openai_low_detail_cost() {
        let text_only = request(vec![parts_msg(vec![text_part("hi")])]);
        let with_low = request(vec![parts_msg(vec![
            text_part("hi"),
            image_part(Some("low")),
        ])]);
        assert_eq!(
            estimate_chat_prompt(&with_low) - estimate_chat_prompt(&text_only),
            LOW_DETAIL_IMAGE_TOKENS
        );
    }

    #[test]
    fn high_and_auto_and_unset_detail_use_the_same_default_estimate() {
        let high = request(vec![parts_msg(vec![image_part(Some("high"))])]);
        let auto = request(vec![parts_msg(vec![image_part(Some("auto"))])]);
        let unset = request(vec![parts_msg(vec![image_part(None)])]);
        assert_eq!(estimate_chat_prompt(&high), estimate_chat_prompt(&auto));
        assert_eq!(estimate_chat_prompt(&auto), estimate_chat_prompt(&unset));
    }

    #[test]
    fn multiple_images_scale_linearly_per_image() {
        let one = request(vec![parts_msg(vec![image_part(None)])]);
        let three = request(vec![parts_msg(vec![
            image_part(None),
            image_part(None),
            image_part(None),
        ])]);
        assert_eq!(
            estimate_chat_prompt(&three) - estimate_chat_prompt(&one),
            2 * DEFAULT_IMAGE_TOKENS
        );
    }
}
