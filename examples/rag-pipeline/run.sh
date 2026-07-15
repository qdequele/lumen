#!/usr/bin/env bash
set -euo pipefail
BASE_URL="${BASE_URL:-http://localhost:8080}"

# The RAG shape has two distinct phases against two different providers:
# embed once at index time (documents go into a vector store), then rerank
# at query time (a shortlist from that vector store gets re-scored against
# the live query). This script mimics both calls against the same 3
# documents so the shapes are easy to compare side by side.

echo "== index time: embed the corpus (OpenAI) =="
curl -sf "$BASE_URL/v1/embeddings" \
  -H 'content-type: application/json' \
  -d '{
    "model": "text-embedding-3-small",
    "input": [
      "Paris is the capital of France.",
      "Berlin is the capital of Germany.",
      "Rome is the capital of Italy."
    ]
  }'
echo

echo "== query time: rerank the shortlist against a query (Cohere), top_n=2 =="
curl -sf "$BASE_URL/v1/rerank" \
  -H 'content-type: application/json' \
  -d '{
    "model": "rerank-english",
    "query": "What is the capital of France?",
    "documents": [
      "Paris is the capital of France.",
      "Berlin is the capital of Germany.",
      "Rome is the capital of Italy."
    ],
    "top_n": 2
  }'
echo
