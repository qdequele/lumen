#!/usr/bin/env bash
set -euo pipefail
BASE_URL="${BASE_URL:-http://localhost:8080}"

echo "== non-streaming chat =="
curl -sf "$BASE_URL/v1/chat/completions" \
  -H 'content-type: application/json' \
  -d '{"model":"gpt-4o","messages":[{"role":"user","content":"Say hello in one word."}]}'
echo

echo "== streaming chat =="
curl -sfN "$BASE_URL/v1/chat/completions" \
  -H 'content-type: application/json' \
  -d '{"model":"gpt-4o","stream":true,"messages":[{"role":"user","content":"Count to three."}]}'
echo
