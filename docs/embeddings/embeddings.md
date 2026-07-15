# Embeddings

`POST /v1/embeddings` speaks the OpenAI request and response format. The
`model` field is one of *your* configured model ids (the `id` in a
`[[providers.models]]` block - see [Providers](../providers.md)).

## Request

```bash
curl -s http://localhost:8080/v1/embeddings \
  -H 'content-type: application/json' \
  -d '{
    "model": "text-embedding-3-small",
    "input": ["the quick brown fox", "a lazy dog"]
  }'
```

## Response

```json
{
  "object": "list",
  "data": [
    { "object": "embedding", "index": 0, "embedding": [0.0023, -0.009, ...] },
    { "object": "embedding", "index": 1, "embedding": [0.0071, 0.014, ...] }
  ],
  "model": "text-embedding-3-small",
  "usage": { "prompt_tokens": 8, "total_tokens": 8 }
}
```

## `encoding_format`

`"encoding_format": "float"` (the default) returns each `embedding` as a
JSON float array. `"base64"` returns it as an OpenAI-style base64 string of
little-endian `f32` bytes instead. This is purely an output concern: LUMEN
always holds vectors as floats internally, so it works uniformly even for
providers with no native `encoding_format`, such as Ollama and TEI.

## Accepted `input` shapes

| Shape | Example | Notes |
|-------|---------|-------|
| Single string | `"input": "hi"` | One item. |
| Array of strings | `"input": ["a", "b"]` | A batch, embedded and returned in order. |
| Pre-tokenized token-id array | `"input": [1, 2, 3]` | One item, counted as one embedding (OpenAI semantics). |
| Batch of token-id arrays | `"input": [[1, 2], [3, 4]]` | Each inner array is one item. |
| Content-part array(s) | `"input": [[{"type": "text", ...}, {"type": "image_url", ...}], "plain text"]` | Multimodal: each item is a string or an array of text/image parts. See [Multimodal embeddings](multimodal.md). |

Pre-tokenized shapes are rejected on providers that cannot consume them (see
below). Content-part shapes require a model that opts into the `image`
modality (see [Multimodal embeddings](multimodal.md)).

## Pre-tokenized input on text-only providers

Token-id array input (`[1,2,3]` or `[[1,2],[3,4]]`) passes through natively
on OpenAI-compatible providers. Providers whose upstream API only accepts
text - `cohere`, `tei`, `ollama`, `jina`, `voyage`, `mistral` - reject it
before any upstream call with `LM-1001` (400), naming the provider and the
rejected shape. See [Error codes](../errors.md).

## Unknown fields and `input_type` (Cohere)

Unlike `/v1/chat/completions`, unknown request fields on `/v1/embeddings` are
captured but never re-serialized into the outgoing provider body - they stop
at the gateway rather than being forwarded, since a strict OpenAI-compatible
upstream may reject fields it does not recognize. The one field the gateway
itself reads is `input_type`, consumed only by the Cohere translation to
override Cohere's query-vs-document intent (`search_query`,
`search_document`, `classification`, `clustering`; defaults to
`search_document`). An unrecognized `input_type` is rejected with `LM-1001`
before any upstream call. See [Providers - cohere](../providers.md#cohere).

## Strict mode

By default a provider silently drops a request field it cannot honor (for
example `dimensions` sent to Ollama, which has no such parameter). Setting
`strict = true` on that provider's `[[providers]]` block makes it reject
such a request instead, with `LM-1001` naming the field. `encoding_format`
is always handled at the response edge and is never affected by `strict`.

## Providers

Which provider kinds serve `embed`, and their batch limits, are in
[Providers](../providers.md).
