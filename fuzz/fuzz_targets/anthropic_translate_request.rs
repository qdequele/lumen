#![no_main]
//! Fuzz Anthropic's `translate_request` (client -> upstream direction):
//! an arbitrary OpenAI-shaped `ChatRequest` (tool calls, tool results,
//! mixed content, arbitrary `extra` passthrough) must translate to the
//! Anthropic wire request - and that request must serialize - without
//! panicking. Reached through the `#[cfg(fuzzing)]` shim in
//! `providers::anthropic::fuzzing` (see that module's doc comment for why
//! the private translate function is exposed this way).
use libfuzzer_sys::fuzz_target;
use lumen_core::ChatRequest;
use lumen_providers::anthropic::fuzzing::fuzz_translate_request;

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }
    // First byte picks streaming vs non-streaming mode; the rest is the
    // candidate request body.
    let stream = data[0] & 1 == 1;
    if let Ok(req) = serde_json::from_slice::<ChatRequest>(&data[1..]) {
        fuzz_translate_request(&req, stream);
    }
});
