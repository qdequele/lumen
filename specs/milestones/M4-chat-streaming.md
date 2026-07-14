# M4 — Chat + SSE streaming

## Objective
Complete `/v1/chat/completions`, zero-copy streaming, Anthropic translation. The most technical milestone.

## Tasks

### 4.1 Non-streaming
- [x] `POST /v1/chat/completions` (stream=false): validation → router → provider → OpenAI response
- [x] Support: messages (system/user/assistant/tool), temperature, max_tokens, stop, tools/tool_choice, response_format json (passthrough via `extra`)

### 4.2 SSE streaming
- [x] `stream=true`: `text/event-stream` response, `data: {...}` chunks, `data: [DONE]` termination
- [x] **Zero-copy passthrough**: when the upstream provider already speaks the OpenAI format (OpenAI, Mistral, Ollama, vLLM), forward the SSE frames as `Bytes` WITHOUT deserializing each chunk (ADR 004; `chat_stream_bytes` + `http::open_stream`). Proven byte-for-byte over 100 chunks.
- [x] Token counting during streaming (ADR 003), "upstream usage" half: passthrough via `stream_options.include_usage` requested automatically; Anthropic/Gemini translation → full usage in the final chunk. *(The "local estimation `estimated=true`" half goes in M5 with the Prometheus counters and `usage_log`.)*
- [x] `stream_options: {include_usage: true}` supported (requested automatically upstream, without overriding a client choice)
- [x] SSE heartbeat (`: ping`) every 15 s (configurable `sse_heartbeat_ms`) if the upstream is silent (keep-alive proxies)
- [x] Client disconnect → drop the stream → immediate reqwest abort (THE LiteLLM lesson #22805): drop-guard moved into the body

### 4.3 Anthropic provider (full translation)
- [x] Request: OpenAI messages → Anthropic format (system extracted to `system`, consecutive tool_results merged into a single user message, OpenAI tools → Anthropic tools + tool_choice, max_tokens mandatory with a default)
- [x] Response: content blocks → OpenAI message (tool_use → tool_calls, arguments re-encoded as a JSON string); stop_reason → finish_reason; usage mapped
- [x] Streaming: Anthropic events (message_start, content_block_delta, message_delta...) → OpenAI chunks, chunk-by-chunk translation via an incremental SSE parser — bounded state, zero buffering of the full content
- [x] Streaming tool_use: content_block_start → tool_call opening, input_json_delta → OpenAI tool_calls delta (index allocated in order of appearance)

### 4.4 Mistral + Google providers
- [x] Mistral: near-passthrough OpenAI (chat + embeddings)
- [x] Google Gemini: `generateContent` (non-streaming) and `streamGenerateContent?alt=sse` (streaming) translated — contents/parts, systemInstruction, usageMetadata, finishReason.

## Acceptance criteria
1. Streaming passthrough: wiremock test sending 100 chunks → the client receives 100 identical byte-for-byte chunks + [DONE]; assertion that no full deserialization takes place (counter in the test code or allocation benchmark).
2. Streaming cancellation: client cuts off after 3 chunks → the wiremock upstream connection is closed in < 100 ms (simulated time).
3. Anthropic round-trip translation: OpenAI request fixture with tools → exact expected Anthropic JSON (snapshot test with insta); same for the response.
4. Anthropic streaming: fixture of Anthropic SSE events (including tool_use) → expected sequence of OpenAI chunks (snapshot).
5. Upstream closes the stream without [DONE] → the client receives an SSE error chunk `data: {"error": {"code": "LM-3010"...}}` then a clean close, no hang.
6. First-token timeout exceeded → 504 LM-3011 (non-streaming) or SSE error (streaming).
7. Token counting (ADR 003): streaming with `include_usage` → in/out tokens from the last chunk, `estimated=false`; streaming WITHOUT upstream usage → out tokens counted locally, `estimated=true`, non-zero response and log. Non-streaming → upstream usage surfaced as-is. *(Satisfied for the "upstream usage" half; local estimation `estimated=true` is verified in M5 with the telemetry infrastructure.)*

## Pitfalls
- The Anthropic streaming translation state must be bounded: NEVER accumulate the full text in memory.
- Upstream SSE frames may be fragmented across TCP packets: the parser must handle incomplete frames (use eventsource-stream or a tested incremental parser).
