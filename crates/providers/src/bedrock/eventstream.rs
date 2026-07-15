//! Incremental decoder for the AWS event-stream framing
//! (`application/vnd.amazon.eventstream`) used by Bedrock `converse-stream`.
//!
//! Unlike SSE, the Converse streaming response is a sequence of length-prefixed
//! binary frames. Each frame is:
//!
//! ```text
//! +----------------+----------------+-------------+---------+---------+-------------+
//! | total len (4)  | headers len(4) | prelude CRC | headers | payload | message CRC |
//! +----------------+----------------+-------------+---------+---------+-------------+
//! ```
//!
//! All integers are big-endian. `total_len` counts the whole frame including
//! the two length fields, both CRCs, headers and payload. Each header is
//! `name_len(1) | name | value_type(1) | value`; Converse only uses string
//! headers (type 7: `value_len(2) | value_bytes`), which is all this decoder
//! needs to read `:event-type`, `:message-type` and `:exception-type`.
//!
//! CRC validation is intentionally skipped (the transport is TLS, so integrity
//! is already assured); the frame LENGTHS are checked exactly so a malformed or
//! truncated frame fails loudly rather than desynchronising the stream. Frames
//! arrive fragmented across TCP packets, so the decoder buffers a partial frame
//! (bounded by [`MAX_FRAME_BYTES`]) until it is complete.

use bytes::Bytes;
use lumen_core::ProviderError;

/// Upper bound on a single buffered frame. Converse event payloads are a few
/// KiB; anything near this is a broken or hostile upstream, and failing beats
/// unbounded growth.
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// The four length/CRC prelude+trailer bytes that are not headers or payload:
/// total_len(4) + headers_len(4) + prelude_crc(4) + message_crc(4).
const OVERHEAD_BYTES: usize = 16;

/// The event-stream string header value type.
const HEADER_TYPE_STRING: u8 = 7;

/// One decoded event-stream message: its string headers and its raw payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct EventMessage {
    /// String-typed headers, in wire order. Non-string headers are skipped.
    pub headers: Vec<(String, String)>,
    /// The message payload (JSON, for Converse events).
    pub payload: Bytes,
}

impl EventMessage {
    /// The value of a header by name, if present.
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// The `:event-type` header (`messageStart`, `contentBlockDelta`, ...).
    pub fn event_type(&self) -> Option<&str> {
        self.header(":event-type")
    }

    /// The `:message-type` header (`event` or `exception`).
    pub fn message_type(&self) -> Option<&str> {
        self.header(":message-type")
    }

    /// The `:exception-type` header, present on error frames.
    pub fn exception_type(&self) -> Option<&str> {
        self.header(":exception-type")
    }
}

/// An incremental event-stream decoder. Feed it raw chunks; collect complete
/// messages.
#[derive(Debug, Default)]
pub(super) struct EventStreamDecoder {
    /// Bytes of the current, not-yet-complete frame (bounded).
    buffer: Vec<u8>,
}

impl EventStreamDecoder {
    /// Create an empty decoder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one raw chunk; return every message it completes.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Translation`] if a frame declares an impossible
    /// length (headers longer than the frame, or a total exceeding
    /// [`MAX_FRAME_BYTES`]) or if a header runs past the header block.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<EventMessage>, ProviderError> {
        self.buffer.extend_from_slice(chunk);
        let mut messages = Vec::new();

        loop {
            // Need at least the 8-byte prelude to know the frame length.
            if self.buffer.len() < 8 {
                break;
            }
            let total_len = u32::from_be_bytes([
                self.buffer[0],
                self.buffer[1],
                self.buffer[2],
                self.buffer[3],
            ]) as usize;
            let headers_len = u32::from_be_bytes([
                self.buffer[4],
                self.buffer[5],
                self.buffer[6],
                self.buffer[7],
            ]) as usize;

            if total_len > MAX_FRAME_BYTES {
                return Err(ProviderError::Translation(format!(
                    "bedrock event-stream frame too large: {total_len} bytes"
                )));
            }
            if total_len < OVERHEAD_BYTES || headers_len > total_len - OVERHEAD_BYTES {
                return Err(ProviderError::Translation(
                    "bedrock event-stream frame has inconsistent lengths".to_owned(),
                ));
            }
            // Wait for the whole frame before decoding it.
            if self.buffer.len() < total_len {
                break;
            }

            // Header block sits after the 12-byte prelude (8 length + 4 CRC);
            // payload fills the rest, before the trailing 4-byte message CRC.
            let headers_start = 12;
            let headers_end = headers_start + headers_len;
            let payload_end = total_len - 4;

            let headers = parse_headers(&self.buffer[headers_start..headers_end])?;
            let payload = Bytes::copy_from_slice(&self.buffer[headers_end..payload_end]);
            messages.push(EventMessage { headers, payload });

            // Drop the consumed frame (CRCs included) and continue.
            self.buffer.drain(..total_len);
        }

        Ok(messages)
    }
}

/// Parse the header block: a sequence of `name_len(1) | name | type(1) | value`.
/// Only string headers (type 7) are read; other types are skipped by their
/// documented fixed or length-prefixed sizes so parsing stays in sync.
fn parse_headers(mut bytes: &[u8]) -> Result<Vec<(String, String)>, ProviderError> {
    let mut headers = Vec::new();
    while !bytes.is_empty() {
        // name_len(1) | name
        let name_len = *bytes.first().ok_or_else(truncated)? as usize;
        bytes = &bytes[1..];
        if bytes.len() < name_len + 1 {
            return Err(truncated());
        }
        let name = String::from_utf8_lossy(&bytes[..name_len]).into_owned();
        bytes = &bytes[name_len..];

        // value_type(1)
        let value_type = bytes[0];
        bytes = &bytes[1..];

        if value_type == HEADER_TYPE_STRING || value_type == 6 {
            // string / byte-array: value_len(2) | value
            if bytes.len() < 2 {
                return Err(truncated());
            }
            let value_len = u16::from_be_bytes([bytes[0], bytes[1]]) as usize;
            bytes = &bytes[2..];
            if bytes.len() < value_len {
                return Err(truncated());
            }
            let value = String::from_utf8_lossy(&bytes[..value_len]).into_owned();
            bytes = &bytes[value_len..];
            headers.push((name, value));
        } else {
            // Non-string header: skip by its fixed width. Converse never sends
            // these on the events we translate, but the sizes keep the parser
            // aligned if one appears.
            let skip = match value_type {
                0 | 1 => 0, // true / false: no value
                2 => 1,     // byte
                3 => 2,     // short
                4 => 4,     // int
                5 | 8 => 8, // long / timestamp
                9 => 16,    // uuid
                _ => return Err(truncated()),
            };
            if bytes.len() < skip {
                return Err(truncated());
            }
            bytes = &bytes[skip..];
        }
    }
    Ok(headers)
}

/// A uniform "header block ran short" translation error (no upstream bytes
/// leaked into the message).
fn truncated() -> ProviderError {
    ProviderError::Translation("bedrock event-stream header block is truncated".to_owned())
}

#[cfg(test)]
#[allow(clippy::cast_possible_truncation)]
pub(super) mod test_support {
    //! Helpers to synthesize event-stream frames for byte-fixture tests, shared
    //! with the streaming-translation tests. The length casts are bounded by
    //! the small fixtures these helpers build.

    /// Encode one string header (`name_len | name | type=7 | value_len | value`).
    fn encode_string_header(name: &str, value: &str) -> Vec<u8> {
        let mut out = Vec::new();
        out.push(name.len() as u8);
        out.extend_from_slice(name.as_bytes());
        out.push(super::HEADER_TYPE_STRING);
        out.extend_from_slice(&(value.len() as u16).to_be_bytes());
        out.extend_from_slice(value.as_bytes());
        out
    }

    /// Build a complete event-stream frame from string headers and a payload.
    /// CRC fields are present (zeroed) but the decoder does not validate them.
    pub fn frame(headers: &[(&str, &str)], payload: &[u8]) -> Vec<u8> {
        let mut header_bytes = Vec::new();
        for (name, value) in headers {
            header_bytes.extend_from_slice(&encode_string_header(name, value));
        }
        let total_len = super::OVERHEAD_BYTES + header_bytes.len() + payload.len();
        let mut out = Vec::with_capacity(total_len);
        out.extend_from_slice(&(total_len as u32).to_be_bytes());
        out.extend_from_slice(&(header_bytes.len() as u32).to_be_bytes());
        out.extend_from_slice(&0u32.to_be_bytes()); // prelude CRC (unchecked)
        out.extend_from_slice(&header_bytes);
        out.extend_from_slice(payload);
        out.extend_from_slice(&0u32.to_be_bytes()); // message CRC (unchecked)
        out
    }

    /// A Converse `event` frame with the given `:event-type` and JSON payload.
    pub fn event_frame(event_type: &str, payload: &str) -> Vec<u8> {
        frame(
            &[
                (":event-type", event_type),
                (":content-type", "application/json"),
                (":message-type", "event"),
            ],
            payload.as_bytes(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{event_frame, frame};
    use super::*;

    #[test]
    fn decodes_a_single_complete_frame() {
        let bytes = event_frame("messageStart", r#"{"role":"assistant"}"#);
        let mut decoder = EventStreamDecoder::new();
        let msgs = decoder.push(&bytes).expect("decodes");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].event_type(), Some("messageStart"));
        assert_eq!(msgs[0].message_type(), Some("event"));
        assert_eq!(&msgs[0].payload[..], br#"{"role":"assistant"}"#);
    }

    #[test]
    fn reassembles_a_frame_split_across_chunks() {
        let bytes = event_frame("contentBlockDelta", r#"{"delta":{"text":"hi"}}"#);
        let mut decoder = EventStreamDecoder::new();
        // Split at an arbitrary interior byte: nothing until the frame completes.
        let (a, b) = bytes.split_at(7);
        assert!(decoder.push(a).expect("partial ok").is_empty());
        let (b1, b2) = b.split_at(5);
        assert!(decoder.push(b1).expect("partial ok").is_empty());
        let msgs = decoder.push(b2).expect("completes");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].event_type(), Some("contentBlockDelta"));
    }

    #[test]
    fn decodes_multiple_frames_from_one_chunk() {
        let mut bytes = event_frame("messageStart", "{}");
        bytes.extend_from_slice(&event_frame("messageStop", r#"{"stopReason":"end_turn"}"#));
        let mut decoder = EventStreamDecoder::new();
        let msgs = decoder.push(&bytes).expect("decodes both");
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].event_type(), Some("messageStart"));
        assert_eq!(msgs[1].event_type(), Some("messageStop"));
    }

    #[test]
    fn exception_frame_exposes_exception_type() {
        let bytes = frame(
            &[
                (":exception-type", "throttlingException"),
                (":message-type", "exception"),
            ],
            br#"{"message":"slow down"}"#,
        );
        let mut decoder = EventStreamDecoder::new();
        let msgs = decoder.push(&bytes).expect("decodes");
        assert_eq!(msgs[0].message_type(), Some("exception"));
        assert_eq!(msgs[0].exception_type(), Some("throttlingException"));
    }

    #[test]
    fn inconsistent_lengths_are_rejected() {
        // headers_len (99) larger than the frame can hold.
        let mut bad = Vec::new();
        bad.extend_from_slice(&20u32.to_be_bytes()); // total
        bad.extend_from_slice(&99u32.to_be_bytes()); // headers len
        bad.extend_from_slice(&[0u8; 12]);
        let mut decoder = EventStreamDecoder::new();
        let err = decoder.push(&bad).expect_err("must reject");
        assert!(matches!(err, ProviderError::Translation(_)));
    }
}
