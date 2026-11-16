//! Mistral content chunks to OpenAI strings.
//!
//! Mistral's reasoning models (Magistral, Mistral Large 4, ...) answer with
//! `content` as an array of typed chunks instead of a string:
//!
//! ```json
//! "content": [
//!   {"type": "thinking", "thinking": [{"type": "text", "text": "..."}]},
//!   {"type": "text", "text": "Hello!"}
//! ]
//! ```
//!
//! Streamed deltas do the same while the model thinks. OpenAI clients expect
//! a string there, and print `[object Object]` (JavaScript) or fail to parse.
//! This module flattens such a `content` to the OpenAI shape: the text chunks
//! joined into `content`, the thinking text in `reasoning_content` (the field
//! DeepSeek, vLLM and others use for the same thing).
//!
//! Only a body or frame that actually carries a content array is parsed and
//! rewritten. Everything else is forwarded as the bytes received (ADR 004,
//! "Mistral content chunks" amendment).

use bytes::Bytes;
use futures::stream::{BoxStream, StreamExt};
use lumen_core::{ChatMessage, ProviderError};
use serde_json::{Map, Value};

use crate::sse::{SseEvent, SseParser};

/// The OpenAI-compatible field that carries a model's reasoning text.
const REASONING_FIELD: &str = "reasoning_content";

/// Whether `bytes` may hold a `"content": [` (a content array). Cheap byte
/// scan, no parsing. A `"content"` key inside a JSON string is escaped
/// (`\"content\"`), so text that merely mentions it does not match; a false
/// positive would only cost a parse that finds nothing to change.
pub(crate) fn has_content_array(bytes: &[u8]) -> bool {
    const KEY: &[u8] = b"\"content\"";
    let mut from = 0;
    while let Some(offset) = bytes
        .get(from..)
        .and_then(|rest| rest.windows(KEY.len()).position(|w| w == KEY))
    {
        let mut rest = bytes.get(from + offset + KEY.len()..).unwrap_or_default();
        rest = trim_json_space(rest);
        if let Some(after_colon) = rest.strip_prefix(b":") {
            if trim_json_space(after_colon).first() == Some(&b'[') {
                return true;
            }
        }
        from += offset + 1;
    }
    false
}

fn trim_json_space(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|b| !matches!(b, b' ' | b'\t' | b'\n' | b'\r'))
        .unwrap_or(bytes.len());
    bytes.get(start..).unwrap_or_default()
}

/// Flatten every `choices[].message.content` array of a non-streaming body.
///
/// A body without a content array is returned as is (same `Bytes`). A body
/// that cannot be parsed is returned as is too: the caller's parse reports
/// the error, as it would have without this step.
pub(crate) fn normalize_completion(body: Bytes) -> Bytes {
    normalize_json(body, "message")
}

/// Strip `reasoning_content` from the messages sent upstream. A client
/// replaying an assistant turn from a reasoning model sends it back, and
/// Mistral rejects fields it does not know.
pub(crate) fn strip_reasoning(messages: &mut [ChatMessage]) {
    for message in messages {
        message.extra.remove(REASONING_FIELD);
    }
}

/// Flatten content arrays in a streamed (SSE) body.
///
/// Fast path: a transport chunk that starts and ends on a frame boundary and
/// holds no content array is forwarded untouched (zero-copy, the common case
/// for every non-reasoning model). Otherwise the chunk goes through the SSE
/// parser, and each complete frame is written back, rewritten if it carries a
/// content array; an incomplete frame waits for the next chunk.
pub(crate) fn normalize_stream(
    bytes: BoxStream<'static, Result<Bytes, ProviderError>>,
) -> BoxStream<'static, Result<Bytes, ProviderError>> {
    bytes
        .scan(SseParser::new(), |parser, item| {
            futures::future::ready(Some(item.and_then(|chunk| normalize_chunk(parser, chunk))))
        })
        .filter(|item| futures::future::ready(!matches!(item, Ok(b) if b.is_empty())))
        .boxed()
}

fn normalize_chunk(parser: &mut SseParser, chunk: Bytes) -> Result<Bytes, ProviderError> {
    if parser.is_idle() && ends_on_frame_boundary(&chunk) && !has_content_array(&chunk) {
        return Ok(chunk);
    }
    let mut out = Vec::new();
    for event in parser.push(&chunk)? {
        write_event(&mut out, &event);
    }
    Ok(Bytes::from(out))
}

/// Whether `chunk` ends with an SSE event terminator (`\n\n` or `\n\r\n`), so
/// an idle parser would be left with nothing buffered.
fn ends_on_frame_boundary(chunk: &[u8]) -> bool {
    chunk.ends_with(b"\n\n") || chunk.ends_with(b"\n\r\n")
}

/// Write one event back in SSE framing, rewriting its data if needed.
fn write_event(out: &mut Vec<u8>, event: &SseEvent) {
    if let Some(name) = &event.event {
        out.extend_from_slice(b"event: ");
        out.extend_from_slice(name.as_bytes());
        out.push(b'\n');
    }
    let rewritten = if has_content_array(event.data.as_bytes()) {
        Some(normalize_json(
            Bytes::copy_from_slice(event.data.as_bytes()),
            "delta",
        ))
    } else {
        None
    };
    let data = rewritten.as_deref().unwrap_or(event.data.as_bytes());
    for line in data.split(|&b| b == b'\n') {
        out.extend_from_slice(b"data: ");
        out.extend_from_slice(line);
        out.push(b'\n');
    }
    out.push(b'\n');
}

/// Flatten `choices[].<field>.content` in a JSON object body.
fn normalize_json(body: Bytes, field: &str) -> Bytes {
    if !has_content_array(&body) {
        return body;
    }
    let Ok(mut value) = serde_json::from_slice::<Value>(&body) else {
        return body;
    };
    let mut changed = false;
    if let Some(choices) = value.get_mut("choices").and_then(Value::as_array_mut) {
        for choice in choices {
            if let Some(object) = choice.get_mut(field).and_then(Value::as_object_mut) {
                changed |= flatten_content(object);
            }
        }
    }
    if !changed {
        return body;
    }
    serde_json::to_vec(&value).map_or(body, Bytes::from)
}

/// Replace a content array by its text, moving thinking text to
/// `reasoning_content`. Returns whether `object` changed.
///
/// A message or delta with no text chunk gets `content: ""`, never `null`:
/// a client that appends every delta's content keeps a clean string.
fn flatten_content(object: &mut Map<String, Value>) -> bool {
    let Some(Value::Array(parts)) = object.get("content") else {
        return false;
    };
    let mut text = String::new();
    let mut reasoning = String::new();
    for part in parts {
        match part.get("type").and_then(Value::as_str) {
            Some("thinking") => push_text(part.get("thinking"), &mut reasoning),
            // `text`, and any other chunk that carries text. Chunks without
            // text (citations' `reference`, ...) have no OpenAI equivalent.
            _ => push_text(part.get("text"), &mut text),
        }
    }
    object.insert("content".to_owned(), Value::String(text));
    if !reasoning.is_empty() {
        match object.get_mut(REASONING_FIELD) {
            Some(Value::String(existing)) => existing.push_str(&reasoning),
            _ => {
                object.insert(REASONING_FIELD.to_owned(), Value::String(reasoning));
            }
        }
    }
    true
}

/// Append the text of `value`: a string, or an array of text chunks.
fn push_text(value: Option<&Value>, out: &mut String) {
    match value {
        Some(Value::String(s)) => out.push_str(s),
        Some(Value::Array(items)) => {
            for item in items {
                match item {
                    Value::String(s) => out.push_str(s),
                    other => {
                        if let Some(s) = other.get("text").and_then(Value::as_str) {
                            out.push_str(s);
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;
    use serde_json::json;

    const REASONING_BODY: &str = r#"{"id":"r","object":"chat.completion","created":1,"model":"mistral-large-4","choices":[{"index":0,"message":{"role":"assistant","content":[{"type":"thinking","closed":true,"thinking":[{"text":"The user said test.","type":"text"}]},{"type":"text","text":"Hello!"}],"tool_calls":null},"finish_reason":"stop"}],"usage":{"prompt_tokens":16,"completion_tokens":87,"total_tokens":103}}"#;

    #[test]
    fn detects_a_content_array_with_any_spacing() {
        assert!(has_content_array(br#"{"content":[{"type":"text"}]}"#));
        assert!(has_content_array(br#"{"content" : [ ]}"#));
        assert!(has_content_array(b"{\"content\":\n  [1]}"));
        assert!(has_content_array(
            br#"{"role":"x","content":"a","b":{"content":[]}}"#
        ));
    }

    #[test]
    fn a_string_content_or_an_escaped_key_is_not_an_array() {
        assert!(!has_content_array(br#"{"content":"[not an array]"}"#));
        assert!(!has_content_array(br#"{"content":null}"#));
        assert!(!has_content_array(br#"{"content":"say \"content\":[x]"}"#));
        assert!(!has_content_array(b""));
    }

    #[test]
    fn splits_thinking_from_text_in_a_completion() {
        let out = normalize_completion(Bytes::from_static(REASONING_BODY.as_bytes()));
        let value: Value = serde_json::from_slice(&out).unwrap();
        let message = &value["choices"][0]["message"];
        assert_eq!(message["content"], "Hello!");
        assert_eq!(message["reasoning_content"], "The user said test.");
        assert_eq!(message["role"], "assistant");
        // Every other field survives.
        assert_eq!(value["usage"]["completion_tokens"], 87);
        assert_eq!(value["choices"][0]["finish_reason"], "stop");
    }

    #[test]
    fn a_string_content_body_is_returned_untouched() {
        let body =
            Bytes::from_static(br#"{"choices":[{"message":{"role":"assistant","content":"hi"}}]}"#);
        let out = normalize_completion(body.clone());
        assert_eq!(out.as_ptr(), body.as_ptr(), "no copy for a string content");
    }

    #[test]
    fn a_body_that_is_not_json_is_returned_untouched() {
        let body = Bytes::from_static(br#"{"content":[ truncated"#);
        assert_eq!(normalize_completion(body.clone()), body);
    }

    #[test]
    fn thinking_only_gives_an_empty_string_content() {
        let mut object = json!({
            "content": [{"type": "thinking", "thinking": [{"type": "text", "text": "hmm"}]}]
        });
        assert!(flatten_content(object.as_object_mut().unwrap()));
        assert_eq!(object["content"], "");
        assert_eq!(object["reasoning_content"], "hmm");
    }

    #[test]
    fn text_chunks_are_joined_in_order() {
        let mut object = json!({
            "content": [
                {"type": "text", "text": "a"},
                {"type": "reference", "reference_ids": [1]},
                {"type": "text", "text": "b"}
            ]
        });
        assert!(flatten_content(object.as_object_mut().unwrap()));
        assert_eq!(object["content"], "ab");
        assert!(object.get("reasoning_content").is_none());
    }

    #[test]
    fn strip_reasoning_removes_the_field_from_replayed_turns() {
        let mut messages: Vec<ChatMessage> = serde_json::from_value(json!([
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": "yo", "reasoning_content": "thought"}
        ]))
        .unwrap();
        strip_reasoning(&mut messages);
        assert!(messages
            .iter()
            .all(|m| !m.extra.contains_key("reasoning_content")));
        assert_eq!(messages[1].role, "assistant");
    }

    async fn run(chunks: Vec<&'static str>) -> Vec<Bytes> {
        let input = stream::iter(
            chunks
                .into_iter()
                .map(|c| Ok(Bytes::from_static(c.as_bytes())))
                .collect::<Vec<_>>(),
        )
        .boxed();
        normalize_stream(input)
            .map(|item| item.unwrap())
            .collect()
            .await
    }

    fn joined(chunks: &[Bytes]) -> String {
        chunks
            .iter()
            .map(|c| std::str::from_utf8(c).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn plain_frames_are_forwarded_as_received() {
        let frame = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hi\"}}]}\n\n";
        let out = run(vec![frame, "data: [DONE]\n\n"]).await;
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], frame.as_bytes());
        assert_eq!(out[1], "data: [DONE]\n\n".as_bytes());
    }

    #[tokio::test]
    async fn thinking_deltas_become_reasoning_content() {
        let out = run(vec![
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":[{\"type\":\"thinking\",\"thinking\":[{\"type\":\"text\",\"text\":\"Let me\"}]}]}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"}}]}\n\n",
            "data: [DONE]\n\n",
        ])
        .await;
        let text = joined(&out);
        let first = text.split("\n\n").next().unwrap();
        let value: Value = serde_json::from_str(first.strip_prefix("data: ").unwrap()).unwrap();
        let delta = &value["choices"][0]["delta"];
        assert_eq!(delta["content"], "");
        assert_eq!(delta["reasoning_content"], "Let me");
        assert_eq!(delta["role"], "assistant");
        assert!(text
            .contains("data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"}}]}\n\n"));
        assert!(text.ends_with("data: [DONE]\n\n"));
        assert!(!text.contains("\"type\":\"thinking\""));
    }

    #[tokio::test]
    async fn a_frame_split_across_chunks_is_rewritten_once_complete() {
        let out = run(vec![
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":[{\"type\":\"te",
            "xt\",\"text\":\"Hi\"}]}}]}\n",
            "\ndata: [DONE]\n\n",
        ])
        .await;
        assert_eq!(
            joined(&out),
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"},\"index\":0}]}\n\ndata: [DONE]\n\n"
        );
    }

    #[tokio::test]
    async fn a_split_plain_frame_is_kept_intact() {
        let out = run(vec![
            "data: {\"choices\":[{\"delta\":{\"content\":\"He",
            "llo\"}}]}\n\ndata: [DONE]\n\n",
        ])
        .await;
        assert_eq!(
            joined(&out),
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\ndata: [DONE]\n\n"
        );
    }

    #[tokio::test]
    async fn usage_frames_survive_a_rewrite() {
        let out = run(vec![
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":[{\"type\":\"text\",\"text\":\"!\"}]},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":1,\"total_tokens\":4}}\n\n",
        ])
        .await;
        let text = joined(&out);
        let value: Value =
            serde_json::from_str(text.trim_end().strip_prefix("data: ").unwrap()).unwrap();
        assert_eq!(value["usage"]["completion_tokens"], 1);
        assert_eq!(value["choices"][0]["delta"]["content"], "!");
        assert_eq!(value["choices"][0]["finish_reason"], "stop");
    }
}
