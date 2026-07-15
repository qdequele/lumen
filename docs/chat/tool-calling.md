# Tool calling

`/v1/chat/completions` accepts OpenAI's `tools` request field and returns
`tool_calls` on the response message, for every chat provider: passed
through untouched for the OpenAI-family kinds and `vllm` (via the same
unknown-field passthrough that carries `tools` and `tool_choice`, see
[Chat completions](completions.md)), translated to and from the provider's
own schema for `anthropic` (`tool_use` content blocks) and `google`
(`tools[].functionDeclarations`, `toolConfig.functionCallingConfig`).

## Two-leg flow

**1. Request with `tools`:**

```json
{
  "model": "gpt-4o",
  "messages": [{ "role": "user", "content": "What is the weather in Paris?" }],
  "tools": [{
    "type": "function",
    "function": {
      "name": "get_weather",
      "parameters": { "type": "object", "properties": { "city": { "type": "string" } } }
    }
  }]
}
```

**2. Response with a tool call** (`finish_reason: "tool_calls"`):

```json
{
  "choices": [{
    "index": 0,
    "message": {
      "role": "assistant",
      "content": null,
      "tool_calls": [{
        "id": "call_1",
        "type": "function",
        "function": { "name": "get_weather", "arguments": "{\"city\":\"Paris\"}" }
      }]
    },
    "finish_reason": "tool_calls"
  }]
}
```

**3. Follow-up request**, appending the assistant's tool call and a `tool`
role message with the result:

```json
{
  "model": "gpt-4o",
  "messages": [
    { "role": "user", "content": "What is the weather in Paris?" },
    { "role": "assistant", "content": null, "tool_calls": [ /* as above */ ] },
    { "role": "tool", "tool_call_id": "call_1", "content": "15C, cloudy" }
  ]
}
```

The model grounds its final answer in the tool result and returns a normal
`finish_reason: "stop"` message.

## Streaming

With `"stream": true`, `tool_calls` arrive as incremental deltas in OpenAI
format (id and name first, then argument fragments), same as OpenAI's own
streaming tool-call shape. See [Streaming](streaming.md).

## Coverage

Translation is implemented for `anthropic` and `google` (Gemini); the
OpenAI-family kinds and `vllm` pass `tools`/`tool_choice` through untouched
since they already speak the OpenAI shape. Provider setup and the full
capability matrix are in [Providers](../providers.md).
