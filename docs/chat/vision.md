# Vision (image input)

`POST /v1/chat/completions` accepts OpenAI's content-parts message shape, so
a user message can carry text and image parts in one array. It is opt-in per
model: a model only accepts image parts once its config declares the `image`
modality (`modalities = ["text", "image"]`; the default is `["text"]`).
`GET /v1/models` reflects the opt-in back as `"modalities": ["text","image"]`
per model.

```json
{
  "model": "gpt-4o",
  "messages": [{
    "role": "user",
    "content": [
      { "type": "text", "text": "What is this?" },
      { "type": "image_url", "image_url": { "url": "data:image/png;base64,iVBORw0KG..." } },
      { "type": "image_url", "image_url": { "url": "https://example.com/cat.png" } }
    ]
  }]
}
```

`image_url.url` is either a `data:<media-type>;base64,<payload>` inline URI
or a remote `http(s)` URL.

## Pre-flight

Sending an image part to a model whose `modalities` lack `"image"` is
rejected with `LM-2003` (400) before any upstream call. The check inspects
the whole fallback chain, not just the primary, so a fallback missing the
`image` modality is caught up front too.

## Per-provider handling

OpenAI-family kinds, `vllm` and `azure` forward image parts verbatim - both
`data:` URIs and remote URLs. `anthropic` translates both forms into its own
schema. `google`, `vertex_ai` and `bedrock` translate only inline `data:`
URIs into their own schema; a remote `http(s)` URL routed to any of the
three is rejected pre-flight with `LM-2004` (400), since none of them
fetches a URL itself and the gateway never fetches a chat image URL on the
caller's behalf (an SSRF vector it deliberately avoids). Full per-kind table
in [Providers - Vision](../providers.md#vision-image-input).

## Provider-native sources

Two provider-native reference forms are recognised in `image_url.url`, for
callers whose images are already uploaded to the provider: an Anthropic
Files API reference (`anthropic-file:<file_id>`) and a Gemini-native
reference (`gs://bucket/object`, or a Gemini Files API URI under
`https://generativelanguage.googleapis.com/`). A reference routed to a model
whose primary provider does not match the reference's own provider is
rejected pre-flight with `LM-2008` (400) instead of surfacing as a confusing
upstream failure. Details, including the `gs://` / Developer API caveat, are
in [Providers - Vision](../providers.md#vision-image-input).

## Token accounting

Upstream-reported `usage` is authoritative and already folds in image
tokens. When an upstream reports no usage at all, the local estimation
fallback counts each image content part with a flat per-image heuristic (85
tokens at `"detail": "low"`, 765 tokens otherwise) rather than counting it as
zero, and the response is still flagged `"estimated": true`. See
[Token accounting](../operations/token-accounting.md).
