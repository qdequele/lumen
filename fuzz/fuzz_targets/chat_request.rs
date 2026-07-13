#![no_main]
//! Fuzz the untrusted client-input boundary: deserializing an OpenAI chat
//! request (with its `extra` passthrough flatten) and re-serializing it must
//! never panic and must round-trip.
use lumen_core::ChatRequest;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(req) = serde_json::from_slice::<ChatRequest>(data) {
        let _ = serde_json::to_vec(&req);
    }
});
