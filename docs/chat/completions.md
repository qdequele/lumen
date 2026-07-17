# Chat completions

`POST /v1/chat/completions` speaks the OpenAI request and response format.
The `model` field is one of *your* configured model ids (the `id` in a
`[[providers.models]]` block, not necessarily the upstream's own model name -
see [Providers](../providers.md) for aliasing with `upstream_id`).

## Request

```bash
curl -s http://localhost:8080/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{
    "model": "gpt-4o",
    "messages": [{"role": "user", "content": "Say hello in one word."}]
  }'
```

## Response

```json
{
  "id": "chatcmpl-abc123",
  "object": "chat.completion",
  "created": 1731000000,
  "model": "gpt-4o",
  "choices": [
    {
      "index": 0,
      "message": { "role": "assistant", "content": "Hello!" },
      "finish_reason": "stop"
    }
  ],
  "usage": { "prompt_tokens": 12, "completion_tokens": 3, "total_tokens": 15 }
}
```

## Unknown fields pass through

Request fields LUMEN does not model as a typed struct field (`tools`,
`response_format`, provider-specific extensions, ...) are preserved verbatim
and forwarded to the upstream untouched, rather than stripped. Provider-specific
parameters keep working without waiting on a LUMEN release to add them by
name.

Verbatim passthrough applies to OpenAI-compatible providers. On the
**translated** kinds (`anthropic`, `google`, `vertex_ai`, `bedrock`,
`cohere`), `response_format`, `seed`, `logprobs`, `top_logprobs`,
`logit_bias` and `parallel_tool_calls`
are mapped natively where the upstream supports them and otherwise dropped
with a debug log - or rejected up front with `LM-1001` when the provider sets
`strict = true`. See the
[chat-extras matrix in Providers](../providers.md#openai-chat-extras-on-translated-providers).

## Routing and request errors

| Code | HTTP | When |
|------|------|------|
| `LM-2001` | 404 | The requested model id was not found. |
| `LM-2002` | 400 | The model exists but does not serve the `chat` capability. |
| `LM-1001` | 400 | Malformed or invalid request body. |
| `LM-1002` | 413 | Request body exceeded the configured size limit. |

Full taxonomy in [Error codes](../errors.md).

## Fallbacks

If the model has a `fallbacks` list and the primary provider fails, the
request fails over automatically. The model that actually served the request
(primary or a fallback) is reported in the `x-lumen-model-used` response
header. See [Resilience](../operations/resilience.md).

## Providers

Which provider kinds serve `chat` and their setup is in
[Providers](../providers.md).
