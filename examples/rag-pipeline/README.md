# rag-pipeline

The two retrieval calls behind a typical RAG pipeline, wired to two
different providers: embeddings via OpenAI (`text-embedding-3-small`) and
reranking via Cohere (`rerank-english`, upstream `rerank-v3.5`). Needs
`OPENAI_API_KEY` and `COHERE_API_KEY`.

## The pipeline story

A RAG pipeline calls the gateway at two different points in a request's
life:

1. **Index time**: documents are embedded once and stored in a vector
   index (outside LUMEN's scope). `run.sh` embeds 3 sample documents to
   show the call shape.
2. **Query time**: a vector search over that index returns a shortlist of
   candidates, then a reranker re-scores that shortlist against the live
   query for a better final ordering. `run.sh` reranks the same 3
   documents against a query with `"top_n": 2`, keeping only the two most
   relevant.

Both capabilities are first-class in LUMEN (see the book's
[Embeddings](../../docs/embeddings/embeddings.md) and
[Reranking](../../docs/reranking/reranking.md) pages), so a RAG pipeline
can point at one gateway for both legs instead of wiring up two separate
SDKs.

## Run it

```bash
# terminal 1 - start the gateway with this scenario's config
export OPENAI_API_KEY=sk-...
export COHERE_API_KEY=...
cargo run -p server -- --config examples/rag-pipeline/config.toml

# terminal 2 - fire the requests
./examples/rag-pipeline/run.sh
```

## Expected output

The embeddings call returns one vector per input document, in the same
order as `input`. The rerank call returns `results`, an array of
`{"index": <original position>, "relevance_score": <float>}` sorted by
descending relevance, truncated to `top_n` (2) entries; the entry for
"Paris is the capital of France." should be first.
