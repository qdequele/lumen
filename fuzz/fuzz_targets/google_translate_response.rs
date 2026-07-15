#![no_main]
//! Fuzz Google's `translate_response` (upstream -> client direction):
//! arbitrary bytes, whether or not they parse as a Gemini response, must
//! never panic. This is the untrusted-upstream boundary - a misbehaving or
//! compromised provider must not be able to crash the gateway. Reached
//! through the `#[cfg(fuzzing)]` shim in `providers::google::fuzzing`.
use libfuzzer_sys::fuzz_target;
use lumen_providers::google::fuzzing::fuzz_translate_response;

fuzz_target!(|data: &[u8]| {
    fuzz_translate_response(data, "gemini-1.5-pro");
});
