# minimal-chat

The smallest possible LUMEN config: one provider (OpenAI), one model
(`gpt-4o`), chat only. Needs `OPENAI_API_KEY` set in the environment.

## What it shows

- Non-streaming chat via `POST /v1/chat/completions`.
- Streaming the same endpoint with `"stream": true`.

## Run it

```bash
# terminal 1 - start the gateway with this scenario's config
export OPENAI_API_KEY=sk-...
cargo run -p server -- --config examples/minimal-chat/config.toml

# terminal 2 - fire the requests
./examples/minimal-chat/run.sh
```

## Expected output

The non-streaming request returns the OpenAI chat completion envelope:

```json
{
  "id": "chatcmpl-...",
  "object": "chat.completion",
  "model": "gpt-4o",
  "choices": [
    { "index": 0, "message": { "role": "assistant", "content": "Hello!" }, "finish_reason": "stop" }
  ],
  "usage": { "prompt_tokens": 13, "completion_tokens": 2, "total_tokens": 15 }
}
```

The streaming request returns a sequence of SSE `data:` frames, each a
`chat.completion.chunk` object, terminated by a literal `data: [DONE]`.
