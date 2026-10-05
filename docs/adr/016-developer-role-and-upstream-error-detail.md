# ADR 016 - The `developer` role and upstream error detail in the log

- Status: accepted
- Date: 2026-10-05
- Amends: ADR 014 (the upstream error body was "only searched, never logged")

## Context

OpenAI added a `developer` message role for instructions; for its models it
is equivalent to `system` (each is mapped to the other depending on the
model). Clients now send it: Meilisearch chat sends its system prompt as a
`developer` message for the `openAi` source with any model not starting with
`gpt-3.5`/`gpt-4`. `ChatMessage.role` is a free string, so the gateway parsed
the role but every translated provider only recognised `system`: Anthropic
received `developer` as a message role and answered 400, surfaced as
`LM-3003` (reproduced on 2026-10-05 against `lumen.meilisearch.com` with
`claude-sonnet-5-5`). OpenAI-compatible hosts without the role (vLLM chat
templates, Mistral, the Azure GA `api-version`) fail the same way.

The failure took long to diagnose because the log carried only
`request failed` with the status: ADR 014 reads the error body to classify
context-length and content-policy refusals, but never logs it.

## Decision

1. **`developer` is `system` wherever the upstream has no `developer` role.**
   `ChatMessage::is_system_role()` (`system` or `developer`) is the only test a
   translator may use for instructions:
   - Anthropic and Bedrock hoist both into the top-level `system`, Gemini and
     Vertex into `systemInstruction`, in message order.
   - Cohere keeps instructions inline and sends `developer` as `system`.
   - OpenAI-compatible pass-through (Mistral, Azure, Ollama chat, Groq,
     Together, vLLM and every other compatible host) rewrites `developer` to
     `system` in place (`developer_role_as_system`).
   - Only OpenAI itself gets `developer` verbatim: the `openai` kind with its
     built-in URL or a `base_url` whose host is `api.openai.com`
     (`OpenAiProvider::with_native_developer_role`). `kind = "openai"` is
     also the documented way to reach unlisted compatible servers (LiteLLM,
     llama.cpp), so any other `base_url` gets the rewrite.
   - Virtual-model presets treat a client `developer` message as a client
     system prompt (`replace` removes it, `if_absent` sees it).
2. **The upstream's error message is logged, bounded and redacted.** The body
   of every upstream client error but 429 is read (still at most 16 KiB and
   500 ms; 5xx bodies stay unread so a stalled body never delays failover).
   `classify_error` logs one `warn` line, `upstream returned an error`, with
   `provider`, `status` and `upstream_error`. That field is built only from
   the vendor's message **string** (`error.message`, `message`,
   `error_description`, string `error`, `detail`, or the same in the first
   item of a JSON array). With no such string nothing is logged: never the
   raw or structured body, which can quote the request (FastAPI/pydantic
   `detail` lists carry an `input` field). The string is cut at the first
   request echo (`'input'`, `"input"`, `input_value`, `input=`), whitespace
   collapsed, probable credentials replaced by `<redacted>` (known key
   prefixes such as `sk-`, `AIza`, `AKIA`, `gsk_`, or a 20+ character
   segment mixing letters and digits), then cut to 512 characters. It sits
   under the request span, so it shares the `request_id` of the
   `request failed` line. The body is still never returned to a client.

## Consequences

- Meilisearch chat with `source: openAi` works against any LUMEN model; the
  `vLlm` source workaround is no longer needed.
- A provider added later must use `is_system_role()`, not `role == "system"`,
  or the regression returns; the shared tests cover each existing translator.
- Vendor error messages describe the request (field paths, limits, model
  ids). Known request-echo shapes are cut and bodies are never logged, but
  a vendor could still phrase a message around a fragment of the input; the
  512-character cut bounds that. An operator who cannot accept it filters
  the event (target `lumen_providers::mapping`) at the subscriber, as
  `docs/operations/logging.md` shows.
- A client that keeps sending requests the upstream rejects now produces
  two lines per request (`warn` detail plus `request failed`).
- Rewriting to `system` loses nothing: OpenAI defines the two roles as
  equivalent, and no other upstream distinguishes them.
