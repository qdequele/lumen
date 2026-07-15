#!/usr/bin/env bash
set -euo pipefail
BASE_URL="${BASE_URL:-http://localhost:8080}"

echo "== chat (Ollama, llama3.2) =="
curl -sf "$BASE_URL/v1/chat/completions" \
  -H 'content-type: application/json' \
  -d '{"model":"llama","messages":[{"role":"user","content":"Say hello in one word."}]}'
echo

echo "== embeddings (Ollama, nomic-embed-text) =="
curl -sf "$BASE_URL/v1/embeddings" \
  -H 'content-type: application/json' \
  -d '{"model":"nomic-embed","input":["the quick brown fox"]}'
echo

echo "== rerank (TEI, bge-reranker-large) =="
if curl -sf "$BASE_URL/v1/rerank" \
  -H 'content-type: application/json' \
  -d '{"model":"bge-reranker","query":"What is the capital of France?","documents":["Paris is the capital of France.","Berlin is in Germany."],"top_n":2}'; then
  echo
else
  echo "skipped (TEI not running on localhost:8081)"
fi
