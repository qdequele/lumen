# Fuzzing

Light fuzzing of the untrusted-input parsers, run weekly in CI (10 min/target)
and on demand locally.

## Targets

- `sse_parser` - the incremental SSE parser (`SseParser::push`), the shared
  byte→event boundary used by both passthrough and translating providers. This
  is the riskiest parsing surface; the Anthropic/Gemini stream translators
  consume its output.
- `chat_request` - deserializing + re-serializing an OpenAI `ChatRequest`,
  exercising the `extra` (unknown-field) passthrough flatten.
- `anthropic_translate_request` / `anthropic_translate_response` - the
  Anthropic provider's `translate_request`/`translate_response` (client<->
  upstream JSON translation), reached through the `#[cfg(fuzzing)]` shim in
  `providers::anthropic::fuzzing`.
- `google_translate_request` / `google_translate_response` - the same for the
  Google (Gemini) provider, via `providers::google::fuzzing`.

## Run locally

```bash
cargo install cargo-fuzz
cargo +nightly fuzz run sse_parser                     -- -max_total_time=600
cargo +nightly fuzz run chat_request                   -- -max_total_time=600
cargo +nightly fuzz run anthropic_translate_request     -- -max_total_time=600
cargo +nightly fuzz run anthropic_translate_response    -- -max_total_time=600
cargo +nightly fuzz run google_translate_request        -- -max_total_time=600
cargo +nightly fuzz run google_translate_response       -- -max_total_time=600
```

Seed the corpus from the repo's fixtures (SSE bodies in the provider tests) for
faster coverage.

## Why `#[cfg(fuzzing)]` shims

`translate_request`/`translate_response` and their wire types
(`AnthropicRequest`, `GeminiResponse`, ...) are private to their provider
module by design - they are an internal implementation detail, not part of
the crate's public API. `cargo fuzz` builds the *entire* dependency graph
(including `lumen-providers`) with `--cfg fuzzing` set via `RUSTFLAGS`, so a
`#[cfg(fuzzing)] pub mod fuzzing { ... }` inside each provider module can
call the private functions directly and stays compiled out of every normal
build (`cargo build`/`test`/`clippy` never set that cfg). This was the least
invasive option considered:

- widening `translate_request`/`translate_response` to `pub(crate)` would
  still leave the wire types unreachable from the standalone `fuzz/`
  workspace without also exposing them;
- a `doc(hidden) pub` re-export would permanently add these internals to the
  crate's public surface, even outside fuzzing builds.

The workspace `Cargo.toml` declares `cfg(fuzzing)` under
`[workspace.lints.rust.unexpected_cfgs]` so referencing it doesn't trip
`cargo clippy --workspace --all-targets -- -D warnings`.
