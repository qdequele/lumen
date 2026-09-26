#![no_main]
//! Fuzz the SystemOne client-input boundary (ADR 013): the hand-written
//! raw-JSON request deserializer, edge validation and re-serialization must
//! never panic, and a request that parsed must re-parse.
use lumen_core::SystemOneRequest;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(req) = serde_json::from_slice::<SystemOneRequest>(data) {
        let _ = req.validate();
        let _ = lumen_core::tokens::estimate_systemone(&req);
        if let Ok(out) = serde_json::to_vec(&req) {
            assert!(serde_json::from_slice::<SystemOneRequest>(&out).is_ok());
        }
    }
});
