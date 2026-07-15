# self-hosted

A fully keyless LUMEN config: no cloud provider, no API key anywhere. Chat
and embeddings come from [Ollama](https://ollama.com), reranking from
[TEI](https://github.com/huggingface/text-embeddings-inference) (Text
Embeddings Inference). Everything runs offline once the models are pulled.

Chat is routed through Ollama's OpenAI-compatible endpoint using the `vllm`
provider kind (it works with any OpenAI-compatible server, not just vLLM),
while embeddings use the native `ollama` kind directly.

## Prerequisites

```bash
# Ollama: chat + embedding models
ollama pull llama3.2
ollama pull nomic-embed-text

# TEI: reranker, published on port 8081 (CPU image; swap for the GPU image
# ghcr.io/huggingface/text-embeddings-inference:1.5 if you have a GPU)
docker run -p 8081:80 -v "$PWD/tei-data:/data" \
  ghcr.io/huggingface/text-embeddings-inference:cpu-1.5 \
  --model-id BAAI/bge-reranker-large
```

Ollama's first call to a model loads it into VRAM, which is why this config
raises `first_token_timeout_ms` and `total_timeout_ms` on the chat provider.

## Run it

```bash
# terminal 1 - start the gateway with this scenario's config
cargo run -p server -- --config examples/self-hosted/config.toml

# terminal 2 - fire the requests
./examples/self-hosted/run.sh
```

## What it shows

- Chat against `llama` (Ollama's `llama3.2`, via the OpenAI-compatible path).
- Embeddings against `nomic-embed` (Ollama's `nomic-embed-text`, native path).
- Reranking against `bge-reranker` (TEI's `BAAI/bge-reranker-large`) with two
  documents.

Rerank is optional: if TEI isn't running on `localhost:8081`, `run.sh`
reports it as skipped instead of failing the whole script, and the chat and
embeddings requests still work on their own.
