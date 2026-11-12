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
    /// Offset in `buffer` below which no event terminator can start: those
    /// bytes were already scanned by an earlier `push`, so a large event
    /// delivered over many reads is scanned once, not once per read.
    scanned: usize,
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
        // Start of the first event not yet consumed. Events are parsed in
        // place and the consumed prefix is removed once, after the loop, so a
        // chunk of many small events does not shift the buffer per event.
        let mut start = 0;
        let mut from = self.scanned;
        // An event ends at a blank line. Scan for the earliest terminator of
        // either style; repeat while complete events remain in the buffer.
        while let Some((end, skip)) = find_event_end(&self.buffer, from) {
            if let Some(event) = parse_event(&self.buffer[start..end]) {
                events.push(event);
            }
            start = end + skip;
            from = start;
        }
        // A terminator is at most three bytes (`\n\r\n`), so one that starts
        // in the last two bytes may still be completed by the next chunk:
        // resume the next scan there, never before the pending event.
        let resume = self.buffer.len().saturating_sub(2).max(start).max(from);
        self.scanned = resume - start;
        if start > 0 {
            self.buffer.drain(..start);
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

/// Find the earliest event terminator starting at or after `from` - a line
/// ending followed by an empty line (`\n\n` or `\n\r\n`; the `\r` of a CRLF
/// field line is stripped by `parse_event`). Returns `(event_end,
/// terminator_len)` as offsets into `buf`.
fn find_event_end(buf: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut i = from;
    while let Some(offset) = buf.get(i..)?.iter().position(|&b| b == b'\n') {
        i += offset;
        match buf.get(i + 1) {
            Some(b'\n') => return Some((i + 1, 1)),
            Some(b'\r') if buf.get(i + 2) == Some(&b'\n') => return Some((i + 1, 2)),
            _ => i += 1,
        }
    }
    None
}

/// Parse one raw event block (without its terminating blank line). Returns
/// `None` for comment-only/empty blocks (e.g. `: keep-alive` pings).
///
/// Fields are borrowed from the buffer while parsing; the only allocations
/// are the owned `event` and `data` of the result (plus one growth per extra
/// line for multi-line data, which LLM providers do not send).
fn parse_event(raw: &[u8]) -> Option<SseEvent> {
    let text = String::from_utf8_lossy(raw);
    let mut event: Option<&str> = None;
    let mut first_data: Option<&str> = None;
    let mut joined: Option<String> = None;

    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if let Some(value) = line.strip_prefix("data:") {
            let value = value.strip_prefix(' ').unwrap_or(value);
            match (first_data, joined.as_mut()) {
                (None, _) => first_data = Some(value),
                (Some(_), Some(all)) => {
                    all.push('\n');
                    all.push_str(value);
                }
                (Some(first), None) => {
                    let mut all = String::with_capacity(first.len() + 1 + value.len());
                    all.push_str(first);
                    all.push('\n');
                    all.push_str(value);
                    joined = Some(all);
                }
            }
        } else if let Some(value) = line.strip_prefix("event:") {
            event = Some(value.strip_prefix(' ').unwrap_or(value));
        }
        // Comments (`:`) and unknown fields (`id:`, `retry:`) are ignored.
    }

    if event.is_none() && first_data.is_none() {
        return None;
    }
    Some(SseEvent {
        event: event.map(str::to_owned),
        data: joined.unwrap_or_else(|| first_data.unwrap_or_default().to_owned()),
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
        let got = push_str(&mut p, "eve");
        assert!(got.is_empty(), "{got:?}");
        let got = push_str(&mut p, "nt: delta\nda");
        assert!(got.is_empty(), "{got:?}");
        let got = push_str(&mut p, "ta: {\"text\":\"hi\"}\n");
        assert!(got.is_empty(), "{got:?}");
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
        let got = push_str(&mut p, ": keep-alive\n\n");
        assert!(got.is_empty(), "{got:?}");
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
        let got = push_str(&mut p, "data: pending\n");
        assert!(got.is_empty(), "{got:?}");
        // Still nothing after another partial line.
        let got = push_str(&mut p, "data: more");
        assert!(got.is_empty(), "{got:?}");
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

    #[test]
    fn large_event_delivered_byte_by_byte_is_parsed_once_complete() {
        // A 256 KiB event arriving one byte per read: every push but the last
        // yields nothing, and the event comes out intact. The scan offset
        // keeps this linear (a rescan from byte 0 per push would be ~3e10
        // byte comparisons here).
        let payload = "x".repeat(256 * 1024);
        let wire = format!("event: big\r\ndata: {payload}\r\n\r\ndata: after\n\n");
        let mut p = SseParser::new();
        let mut events = Vec::new();
        for byte in wire.as_bytes() {
            events.extend(p.push(std::slice::from_ref(byte)).expect("push succeeds"));
        }
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event.as_deref(), Some("big"));
        assert_eq!(events[0].data, payload);
        assert_eq!(events[1].event, None);
        assert_eq!(events[1].data, "after");
    }

    #[test]
    fn terminator_split_at_every_position_yields_the_same_events() {
        // Both terminator styles, cut at every byte offset: the scan offset
        // must never skip a terminator whose bytes straddle two pushes.
        for wire in [
            "data: a\n\ndata: b\n\n",
            "data: a\r\n\r\ndata: b\r\n\r\n",
            "data: a\n\r\ndata: b\r\n\n",
            "data: a\ndata: a2\n\n: ping\n\ndata: b\n\n",
        ] {
            let mut whole = SseParser::new();
            let expected = push_str(&mut whole, wire);
            for cut in 0..=wire.len() {
                let mut p = SseParser::new();
                let mut got = p.push(&wire.as_bytes()[..cut]).expect("push succeeds");
                got.extend(p.push(&wire.as_bytes()[cut..]).expect("push succeeds"));
                assert_eq!(got, expected, "wire {wire:?} cut at {cut}");
            }
        }
    }

    #[test]
    fn utf8_code_point_split_across_chunks_is_preserved() {
        let wire = "data: caf\u{e9} \u{1f600}\n\n".as_bytes();
        for cut in 0..=wire.len() {
            let mut p = SseParser::new();
            let mut got = p.push(&wire[..cut]).expect("push succeeds");
            got.extend(p.push(&wire[cut..]).expect("push succeeds"));
            assert_eq!(got.len(), 1, "cut at {cut}");
            assert_eq!(got[0].data, "caf\u{e9} \u{1f600}");
        }
    }
}
