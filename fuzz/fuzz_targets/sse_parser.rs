#![no_main]
//! Fuzz the incremental SSE parser (the shared byte→event boundary used by the
//! passthrough and translating providers). Arbitrary bytes, split into two
//! chunks to exercise the cross-chunk buffer, must never panic.
use lumen_providers::sse::SseParser;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut parser = SseParser::new();
    let mid = data.len() / 2;
    let _ = parser.push(&data[..mid]);
    let _ = parser.push(&data[mid..]);
});
