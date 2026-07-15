# Batching

An embed request with more inputs than the target provider's batch limit is
split into sub-batches, run with bounded concurrency, and reassembled in the
original input order before the response is returned. This is invisible to
the client: one request in, one response out, `data[].index` numbered
against the original input regardless of how it was split upstream.

## Where limits come from

Every provider kind has a built-in embed batch limit. The native kinds
(`openai`, `mistral`, `cohere`, `jina`, `voyage`, `tei`, `ollama`) each have
their own limit; the OpenAI-compatible hosts all share a 2048-input limit.
The exact numbers are in the [Providers](../providers.md) matrix, in the
native-kinds table and in the
[OpenAI-compatible hosts](../providers.md#openai-compatible-hosts) section.

## Usage

A batched request still produces exactly one `usage` object: `prompt_tokens`
and `total_tokens` are summed across every sub-batch response before the
client sees them.
