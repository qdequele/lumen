# Streaming

Add `"stream": true` to a `/v1/chat/completions` request and the response
becomes `text/event-stream`: a series of `data: {...}` frames (each a
`chat.completion.chunk`), terminated by a literal `data: [DONE]`.

```bash
curl -s http://localhost:8080/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{
    "model": "gpt-4o",
    "messages": [{"role": "user", "content": "Count to 3."}],
    "stream": true
  }'
```

```
data: {"id":"chatcmpl-abc123","object":"chat.completion.chunk","created":1731000000,"model":"gpt-4o","choices":[{"index":0,"delta":{"role":"assistant","content":"1"},"finish_reason":null}]}

data: {"id":"chatcmpl-abc123","object":"chat.completion.chunk","created":1731000000,"model":"gpt-4o","choices":[{"index":0,"delta":{"content":", 2, 3"},"finish_reason":null}]}

data: {"id":"chatcmpl-abc123","object":"chat.completion.chunk","created":1731000000,"model":"gpt-4o","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}

data: [DONE]
```

## Passthrough design

When the upstream already speaks OpenAI-shaped SSE (the OpenAI-family kinds,
`vllm`), the gateway forwards the upstream bytes to the client verbatim - no
per-chunk deserialize/re-serialize round trip. Providers that translate a
foreign event schema (`anthropic`, `google`) build typed chunks instead,
which is not zero-copy but is required because they must translate the
schema anyway. See [ADR 004](../adr/004-streaming-passthrough.md).

## Heartbeats

If the stream goes idle for longer than `sse_heartbeat_ms`
(`[server]` in `config.example.toml`), the gateway injects a `: ping` SSE
comment to keep intermediate proxies from reaping the connection as silent.

## Guards

- No first SSE frame within `first_token_timeout_ms` (`[server]`) -> the
  request fails with `LM-3011` (504).
- The upstream connection dies mid-stream without a `[DONE]` terminator ->
  the client receives a terminal SSE error frame carrying `LM-3010` (502).

Both are documented in [Error codes](../errors.md).

## Client disconnect

If the client disconnects mid-stream, the upstream call is aborted and the
request's accounting settles at HTTP 499 (`LM-6001`, `client_cancelled`) -
never counted as a `5xx` internal error. See
[ADR 006](../adr/006-client-cancellation-error-code.md) and
[Error codes](../errors.md).

## Usage in streams

The final chunk carries `usage` when the upstream reports it there;
otherwise the gateway falls back to a local estimate flagged
`"estimated": true`. Every request produces a token count, never a silent
zero - see [Token accounting](../operations/token-accounting.md).
