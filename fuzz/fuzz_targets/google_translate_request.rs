#![no_main]
//! Fuzz Google's `translate_request` (client -> upstream direction): an
//! arbitrary OpenAI-shaped `ChatRequest` (image parts, tool traffic,
//! arbitrary `extra`) must translate to the Gemini wire request - and that
//! request must serialize - without panicking. Reached through the
//! `#[cfg(fuzzing)]` shim in `providers::google::fuzzing`.
use libfuzzer_sys::fuzz_target;
use lumen_core::ChatRequest;
use lumen_providers::google::fuzzing::fuzz_translate_request;

fuzz_target!(|data: &[u8]| {
    if let Ok(req) = serde_json::from_slice::<ChatRequest>(data) {
        fuzz_translate_request(&req);
    }
});
