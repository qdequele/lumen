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

## Run locally

```bash
cargo install cargo-fuzz
cargo +nightly fuzz run sse_parser   -- -max_total_time=600
cargo +nightly fuzz run chat_request -- -max_total_time=600
```

Seed the corpus from the repo's fixtures (SSE bodies in the provider tests) for
faster coverage.

## Follow-up

Fuzzing the Anthropic/Gemini *event translation* directly (not just via the SSE
parser) needs a small public shim over the currently-private `translate_*`
functions; tracked in `docs/backlog.md`.
