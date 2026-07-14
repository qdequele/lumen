//! Incremental Server-Sent-Events parser for upstream streams.
//!
//! Upstream SSE frames arrive fragmented across arbitrary TCP packet
//! boundaries: one `Bytes` chunk may hold half an event, three events, or an
//! event split in the middle of its `data:` line. This parser buffers only the
//! **current incomplete event** (bounded - see [`MAX_EVENT_BYTES`]) and yields
//! complete events as they close, so translating providers (Anthropic, Google)
//! can consume upstream streams without ever holding the full response in
//! memory.
//!
//! Scope: exactly what LLM provider streams need - `event:` and `data:` fields
//! (multi-line `data:` joined with `\n`), comment lines (`:`) ignored, LF and
//! CRLF line endings. `id:`/`retry:` fields are ignored.

use lumen_core::ProviderError;

/// Upper bound on one buffered event. A single SSE event from an LLM provider
/// is a few KiB; anything approaching this limit is a broken or hostile
/// upstream, and failing beats unbounded memory growth.
const MAX_EVENT_BYTES: usize = 1024 * 1024;

/// One complete SSE event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    /// The `event:` field, if present (e.g. Anthropic's `content_block_delta`).
    pub event: Option<String>,
    /// The `data:` payload; multi-line data joined with `\n`.
    pub data: String,
}

/// An incremental SSE parser. Feed it raw chunks; collect complete events.
#[derive(Debug, Default)]
pub struct SseParser {
    /// Bytes of the current, not-yet-terminated event (bounded).
    buffer: Vec<u8>,
}

impl SseParser {
    /// Create an empty parser.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one raw chunk; return every event completed by it.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Translation`] if a single event exceeds
    /// [`MAX_EVENT_BYTES`] without terminating.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<SseEvent>, ProviderError> {
        self.buffer.extend_from_slice(chunk);

        let mut events = Vec::new();
        // An event ends at a blank line. Scan for the earliest terminator of
        // either style; repeat while complete events remain in the buffer.
        while let Some((end, skip)) = find_event_end(&self.buffer) {
            let raw: Vec<u8> = self.buffer.drain(..end + skip).collect();
            if let Some(event) = parse_event(&raw[..end]) {
                events.push(event);
            }
        }

        // The cap applies to what is left INCOMPLETE after draining: a large
        // transport chunk made of many small complete events is fine; a single
        // never-terminating event is not.
        if self.buffer.len() > MAX_EVENT_BYTES {
            return Err(ProviderError::Translation(
                "SSE event exceeds the maximum buffered size".to_owned(),
            ));
        }
        Ok(events)
    }
}

/// Find the earliest event terminator - a line ending followed by an empty
/// line (`\n\n` or `\n\r\n`; the `\r` of a CRLF field line is stripped by
/// `parse_event`). Returns `(event_end, terminator_len)`.
fn find_event_end(buf: &[u8]) -> Option<(usize, usize)> {
    for (i, pair) in buf.windows(2).enumerate() {
        if pair[0] != b'\n' {
            continue;
        }
        if pair[1] == b'\n' {
            return Some((i + 1, 1));
        }
        if pair[1] == b'\r' && buf.get(i + 2) == Some(&b'\n') {
            return Some((i + 1, 2));
        }
    }
    None
}

/// Parse one raw event block (without its terminating blank line). Returns
/// `None` for comment-only/empty blocks (e.g. `: keep-alive` pings).
fn parse_event(raw: &[u8]) -> Option<SseEvent> {
    let text = String::from_utf8_lossy(raw);
    let mut event: Option<String> = None;
    let mut data_lines: Vec<&str> = Vec::new();

    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if let Some(value) = line.strip_prefix("data:") {
            data_lines.push(value.strip_prefix(' ').unwrap_or(value));
        } else if let Some(value) = line.strip_prefix("event:") {
            event = Some(value.strip_prefix(' ').unwrap_or(value).to_owned());
        }
        // Comments (`:`) and unknown fields (`id:`, `retry:`) are ignored.
    }

    if event.is_none() && data_lines.is_empty() {
        return None;
    }
    Some(SseEvent {
        event,
        data: data_lines.join("\n"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push_str(parser: &mut SseParser, s: &str) -> Vec<SseEvent> {
        parser.push(s.as_bytes()).expect("push succeeds")
    }

    #[test]
    fn parses_a_single_complete_event() {
        let mut p = SseParser::new();
        let events = push_str(&mut p, "event: message_start\ndata: {\"a\":1}\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("message_start"));
        assert_eq!(events[0].data, "{\"a\":1}");
    }

    #[test]
    fn parses_multiple_events_in_one_chunk() {
        let mut p = SseParser::new();
        let events = push_str(&mut p, "data: one\n\ndata: two\n\ndata: three\n\n");
        assert_eq!(
            events.iter().map(|e| e.data.as_str()).collect::<Vec<_>>(),
            vec!["one", "two", "three"]
        );
    }

    #[test]
    fn reassembles_an_event_fragmented_across_chunks() {
        let mut p = SseParser::new();
        // Split in the middle of the field name, the payload, and the terminator.
        assert!(push_str(&mut p, "eve").is_empty());
        assert!(push_str(&mut p, "nt: delta\nda").is_empty());
        assert!(push_str(&mut p, "ta: {\"text\":\"hi\"}\n").is_empty());
        let events = push_str(&mut p, "\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("delta"));
        assert_eq!(events[0].data, "{\"text\":\"hi\"}");
    }

    #[test]
    fn handles_crlf_line_endings() {
        let mut p = SseParser::new();
        let events = push_str(&mut p, "event: ping\r\ndata: {}\r\n\r\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("ping"));
        assert_eq!(events[0].data, "{}");
    }

    #[test]
    fn joins_multi_line_data_with_newlines() {
        let mut p = SseParser::new();
        let events = push_str(&mut p, "data: line1\ndata: line2\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "line1\nline2");
    }

    #[test]
    fn ignores_comment_only_blocks() {
        let mut p = SseParser::new();
        assert!(push_str(&mut p, ": keep-alive\n\n").is_empty());
        // A comment before a real event does not pollute it.
        let events = push_str(&mut p, ": ping\ndata: real\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "real");
    }

    #[test]
    fn data_without_space_after_colon_is_accepted() {
        let mut p = SseParser::new();
        let events = push_str(&mut p, "data:tight\n\n");
        assert_eq!(events[0].data, "tight");
    }

    #[test]
    fn incomplete_event_stays_buffered() {
        let mut p = SseParser::new();
        assert!(push_str(&mut p, "data: pending\n").is_empty());
        // Still nothing after another partial line.
        assert!(push_str(&mut p, "data: more").is_empty());
        let events = push_str(&mut p, "\n\n");
        assert_eq!(events[0].data, "pending\nmore");
    }

    #[test]
    fn large_chunk_of_many_small_complete_events_is_accepted() {
        // The cap targets one runaway event, not the transport chunk size.
        let mut p = SseParser::new();
        let one = "data: x\n\n";
        let big = one.repeat(MAX_EVENT_BYTES / one.len() + 1);
        let events = p.push(big.as_bytes()).expect("complete events drain");
        assert_eq!(events.len(), MAX_EVENT_BYTES / one.len() + 1);
    }

    #[test]
    fn oversized_event_is_rejected() {
        let mut p = SseParser::new();
        let big = vec![b'x'; MAX_EVENT_BYTES + 1];
        assert!(matches!(p.push(&big), Err(ProviderError::Translation(_))));
    }
}
