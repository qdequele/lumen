//! Google Gemini streaming translation (`streamGenerateContent?alt=sse`).
//!
//! Gemini streams SSE events whose `data:` payloads are partial
//! `GenerateContentResponse` objects: each carries the next slice of candidate
//! text in `candidates[0].content.parts[].text`; the final one adds a
//! `finishReason` and cumulative `usageMetadata`. There is no explicit
//! terminal event — the translator marks the stream done when it sees a
//! `finishReason`, so an upstream that dies earlier leaves no terminator
//! (FG-3010 downstream). Translation state is bounded: a first-chunk flag and
//! the ids only — content is never accumulated.

use ferrogate_core::{ChatChunk, ChatChunkChoice, ChatDelta, ProviderError, Usage};
use serde::Deserialize;

use super::map_finish_reason;
use crate::chat::{SseTranslator, StreamItem};
use crate::sse::SseEvent;

/// Translator state for one Gemini stream. Bounded by construction.
pub(super) struct GoogleTranslator {
    /// The client-requested model (Gemini events do not repeat it).
    model: String,
    /// Whether the initial `role: assistant` chunk was emitted yet.
    started: bool,
    /// Set once a `finishReason` was seen; later events are ignored.
    finished: bool,
}

impl GoogleTranslator {
    pub(super) fn new(requested_model: &str) -> Self {
        Self {
            model: requested_model.to_owned(),
            started: false,
            finished: false,
        }
    }

    fn chunk(
        &self,
        delta: ChatDelta,
        finish_reason: Option<String>,
        usage: Option<Usage>,
    ) -> ChatChunk {
        ChatChunk {
            id: String::new(),
            object: "chat.completion.chunk".to_owned(),
            created: 0, // Gemini does not stream a creation timestamp.
            model: self.model.clone(),
            choices: vec![ChatChunkChoice {
                index: 0,
                delta,
                finish_reason,
            }],
            usage,
        }
    }
}

impl SseTranslator for GoogleTranslator {
    fn translate(&mut self, event: &SseEvent) -> Result<Vec<StreamItem>, ProviderError> {
        if self.finished || event.data.is_empty() {
            return Ok(Vec::new());
        }
        let parsed: GeminiStreamChunk = serde_json::from_str(&event.data)
            .map_err(|e| ProviderError::Translation(format!("gemini stream event: {e}")))?;

        let mut items = Vec::new();
        if !self.started {
            self.started = true;
            items.push(StreamItem::Chunk(self.chunk(
                ChatDelta {
                    role: Some("assistant".to_owned()),
                    content: Some(String::new()),
                    extra: serde_json::Map::new(),
                },
                None,
                None,
            )));
        }

        let Some(candidate) = parsed.candidates.into_iter().next() else {
            // Usage-only / safety-only fragments carry nothing to forward.
            return Ok(items);
        };

        let text: String = candidate
            .content
            .parts
            .into_iter()
            .map(|p| p.text)
            .collect();
        let finish = map_finish_reason(candidate.finish_reason.as_deref());

        if !text.is_empty() {
            items.push(StreamItem::Chunk(self.chunk(
                ChatDelta {
                    role: None,
                    content: Some(text),
                    extra: serde_json::Map::new(),
                },
                None,
                None,
            )));
        }

        if finish.is_some() {
            // The final fragment: emit the finish/usage chunk, then terminate.
            let usage = parsed.usage_metadata.map(|u| Usage {
                prompt_tokens: u.prompt,
                completion_tokens: u.candidates,
                total_tokens: u.total,
                estimated: None,
            });
            items.push(StreamItem::Chunk(self.chunk(
                ChatDelta::default(),
                finish,
                usage,
            )));
            self.finished = true;
            items.push(StreamItem::Done);
        }
        Ok(items)
    }
}

// ---- Wire types (streaming fragments) --------------------------------------

#[derive(Deserialize)]
struct GeminiStreamChunk {
    #[serde(default)]
    candidates: Vec<GeminiStreamCandidate>,
    #[serde(rename = "usageMetadata", default)]
    usage_metadata: Option<GeminiStreamUsage>,
}

#[derive(Deserialize)]
struct GeminiStreamCandidate {
    #[serde(default)]
    content: GeminiStreamContent,
    #[serde(rename = "finishReason", default)]
    finish_reason: Option<String>,
}

#[derive(Default, Deserialize)]
struct GeminiStreamContent {
    #[serde(default)]
    parts: Vec<GeminiStreamPart>,
}

#[derive(Deserialize)]
struct GeminiStreamPart {
    #[serde(default)]
    text: String,
}

#[derive(Deserialize)]
struct GeminiStreamUsage {
    #[serde(rename = "promptTokenCount", default)]
    prompt: u32,
    #[serde(rename = "candidatesTokenCount", default)]
    candidates: u32,
    #[serde(rename = "totalTokenCount", default)]
    total: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    // `json!` literals read better passed by value in test fixtures.
    #[allow(clippy::needless_pass_by_value)]
    fn event(data: serde_json::Value) -> SseEvent {
        SseEvent {
            event: None,
            data: data.to_string(),
        }
    }

    #[test]
    fn stream_fragments_translate_to_role_text_finish_and_done() {
        let mut t = GoogleTranslator::new("gemini-2.0");
        let first = t
            .translate(&event(serde_json::json!({
                "candidates": [
                    { "content": { "parts": [{ "text": "Hello" }], "role": "model" } }
                ]
            })))
            .expect("translates");
        // First fragment: role chunk + text delta.
        assert_eq!(first.len(), 2);
        let StreamItem::Chunk(role) = &first[0] else {
            panic!("expected chunk")
        };
        assert_eq!(role.choices[0].delta.role.as_deref(), Some("assistant"));
        assert_eq!(role.model, "gemini-2.0");
        let StreamItem::Chunk(text) = &first[1] else {
            panic!("expected chunk")
        };
        assert_eq!(text.choices[0].delta.content.as_deref(), Some("Hello"));

        let last = t
            .translate(&event(serde_json::json!({
                "candidates": [{
                    "content": { "parts": [{ "text": " world" }], "role": "model" },
                    "finishReason": "STOP"
                }],
                "usageMetadata": {
                    "promptTokenCount": 6, "candidatesTokenCount": 2, "totalTokenCount": 8
                }
            })))
            .expect("translates");
        // Final fragment: text delta + finish chunk (with usage) + Done.
        assert_eq!(last.len(), 3);
        let StreamItem::Chunk(text) = &last[0] else {
            panic!("expected chunk")
        };
        assert_eq!(text.choices[0].delta.content.as_deref(), Some(" world"));
        let StreamItem::Chunk(finish) = &last[1] else {
            panic!("expected chunk")
        };
        assert_eq!(finish.choices[0].finish_reason.as_deref(), Some("stop"));
        let usage = finish.usage.expect("usage");
        assert_eq!(usage.prompt_tokens, 6);
        assert_eq!(usage.completion_tokens, 2);
        assert_eq!(usage.total_tokens, 8);
        assert_eq!(last[2], StreamItem::Done);
    }

    #[test]
    fn max_tokens_finish_maps_to_length() {
        let mut t = GoogleTranslator::new("g");
        let items = t
            .translate(&event(serde_json::json!({
                "candidates": [{
                    "content": { "parts": [] },
                    "finishReason": "MAX_TOKENS"
                }]
            })))
            .expect("translates");
        // role + finish + Done (no text).
        let StreamItem::Chunk(finish) = &items[1] else {
            panic!("expected chunk")
        };
        assert_eq!(finish.choices[0].finish_reason.as_deref(), Some("length"));
    }

    #[test]
    fn fragments_after_finish_are_ignored() {
        let mut t = GoogleTranslator::new("g");
        let _ = t
            .translate(&event(serde_json::json!({
                "candidates": [{ "content": { "parts": [] }, "finishReason": "STOP" }]
            })))
            .expect("ok");
        let late = t
            .translate(&event(serde_json::json!({
                "candidates": [{ "content": { "parts": [{ "text": "late" }] } }]
            })))
            .expect("ok");
        assert!(late.is_empty());
    }

    #[test]
    fn malformed_fragment_is_a_translation_error() {
        let mut t = GoogleTranslator::new("g");
        let err = t
            .translate(&SseEvent {
                event: None,
                data: "not json".to_owned(),
            })
            .expect_err("fails");
        assert!(matches!(err, ProviderError::Translation(_)));
    }

    #[test]
    fn candidate_less_keepalive_fragments_forward_nothing() {
        let mut t = GoogleTranslator::new("g");
        let first = t.translate(&event(serde_json::json!({}))).expect("ok");
        // Only the initial role chunk, no content.
        assert_eq!(first.len(), 1);
        let again = t.translate(&event(serde_json::json!({}))).expect("ok");
        assert!(again.is_empty());
    }
}
