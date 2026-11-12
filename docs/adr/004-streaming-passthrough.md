# ADR 004 - Zero-copy SSE streaming vs. the typed chunk trait

- Status: accepted
- Date: 2026-07-12

## Context

`CLAUDE.md` sketches `ChatProvider::chat_stream -> BoxStream<ChatChunk>`: a
stream of *typed* chunks. But the streaming spec (§4.2) demands **zero-copy
passthrough** - when the upstream already speaks OpenAI SSE (OpenAI, Mistral,
Ollama, vLLM), the gateway must forward the SSE frames as `Bytes` **without
deserializing each chunk**. Deserializing a `ChatChunk` and re-serializing it
per frame is exactly the per-token allocation overhead that makes LiteLLM
1.7–4× slower; it violates pillar 1 (< 1 ms added p99).

So the typed stream and the zero-copy requirement pull in opposite directions.
Translating providers (Anthropic, Gemini) genuinely need typed chunks - they
build OpenAI chunks from a foreign event schema. Passthrough providers must
not pay for typing they don't need.

## Decision

Both paths converge on a single server contract: a provider yields the
**complete SSE response body as a `Bytes` stream** - framing (`data: …\n\n`) and
the terminal `data: [DONE]\n\n` included. The server pipes that byte stream
straight into the HTTP response body; it does not re-frame, and for passthrough
it does not deserialize.

Concretely, add one method to `ChatProvider`:

```rust
async fn chat_stream_bytes(&self, req, cancel)
    -> Result<BoxStream<'static, Result<Bytes, ProviderError>>, ProviderError>;
```

with a **default** implementation that adapts the typed `chat_stream`: serialize
each `ChatChunk` to `data: {json}\n\n` and append `data: [DONE]\n\n`. Providers
that translate a foreign schema (Anthropic) inherit the default - correct, if
not zero-copy, which is fine because they must build chunks anyway.

Passthrough providers (**OpenAI, Mistral**) **override** `chat_stream_bytes`:
set `stream: true` upstream, send, and on success return
`reqwest::Response::bytes_stream()` mapped to `ProviderError` - the upstream's
own bytes, verbatim, `[DONE]` and all. No `serde` round-trip on the hot path.

### Errors and cancellation
- Failures *before* the stream (non-2xx status, transport) surface as a normal
  `Err` → JSON error envelope, exactly like non-streaming. Only a *mid-stream*
  failure becomes an SSE error frame.
- The server holds the per-request cancel drop-guard **inside** the body stream,
  so a client disconnect drops the body, drops the guard and the underlying
  reqwest byte stream, closing the upstream connection promptly (the LiteLLM
  #22805 lesson). The initial send is wrapped in `with_cancel` too.

### Usage / token accounting (ADR 003)
Pure passthrough does not deserialize, so streaming usage is sniffed off the
final frame opportunistically (or estimated) in the token-accounting path -
it must never block or re-serialize the passthrough path.

## Consequences

- OpenAI/Mistral streaming is true zero-copy: upstream bytes → client bytes, one
  bounded copy through the socket, no per-chunk `serde`.
- Anthropic/Gemini keep the typed path and get correct (non-passthrough) SSE via
  the default adapter.
- The server's streaming handler no longer uses axum's `Sse` type; it writes a
  raw `Bytes` body with `content-type: text/event-stream`. SSE heartbeats move
  to an explicit injected frame rather than axum's `KeepAlive`.
- `core` gains a `bytes` dependency (already ubiquitous via reqwest) for the
  trait's return type.

## Amendment 2026-11-12 - non-streaming responses are forwarded as bytes too

The decision above covered `stream: true` only. A non-streaming completion
from a same-schema upstream was still parsed into `ChatResponse` and
serialized again, although the gateway reads nothing from it but `usage`.

### Chat completions

`ChatProvider` gains `chat_completion`, which returns a `ChatCompletion`:
`Typed(ChatResponse)` or `Passthrough` (the upstream body as `Bytes` plus the
`Usage` read from it). The default wraps `chat`, so translating providers
(Anthropic, Google, Vertex, Bedrock, Cohere) are unchanged. The
OpenAI-compatible providers (`openai` and the kinds built on it, `mistral`,
`azure`) override it with `ChatCompletion::from_openai_body`. `chat` itself
stays typed: the registry probes and the typed streaming fallback use it.

**When a body is forwarded as bytes.** Only when nothing in it has to change,
which today means all of:

- the upstream speaks the OpenAI schema (the provider overrides
  `chat_completion`);
- the body reports usable usage: `usage.prompt_tokens > 0` or
  `usage.completion_tokens > 0`, the same test the handler already used to
  tell reported usage from none (ADR 003);
- the body has a `choices` array.

In every other case the body is parsed into `ChatResponse` as before, and the
handler adds the flagged estimate (`usage.estimated: true`). The handler never
rewrites any other field of a non-streaming completion (the response `model`
is the upstream's, also for a virtual model), so no other condition applies.
A future feature that edits the response body must take the typed path, or
extend the conditions here.

**What is probed.** One `serde_json` pass over the body that builds only the
`Usage` (with its `prompt_tokens_details` / `completion_tokens_details`
breakdown, issue #99) and checks that `choices` is an array. Every other value
is skipped without being built. The pass still rejects a body that is not
well-formed JSON, including a truncated one. A probe that fails, or finds no
usable usage, falls back to the full typed parse, whose error is the
`LM-3002` reported before: a `200` that carries usage but no `choices` is
still an upstream fault.

**What changes for clients.** A forwarded body is byte-identical to the
upstream's. The typed path dropped fields it has no slot for (unknown keys
inside `usage` or a choice, such as a provider's `cost` or `logprobs`) and
normalised spacing and key order; a forwarded body keeps them. The response
headers are unchanged (`content-type: application/json`, the routing headers,
`x-lumen-cost-micro-usd`).

**What is no longer checked.** A forwarded body is not validated against the
`ChatResponse` shape beyond the two probed fields (`id`, `object`, `created`,
`model` and the choice contents are not inspected), as a streamed body never
was. Token accounting is unaffected: it reads the same `Usage` either way.
