#!/usr/bin/env bash
set -euo pipefail
BASE_URL="${BASE_URL:-http://localhost:8080}"

echo "== chat with fallback (watch x-lumen-model-used) =="
response=$(curl -sf -D - "$BASE_URL/v1/chat/completions" \
  -H 'content-type: application/json' \
  -d '{"model":"gpt-4o","messages":[{"role":"user","content":"Say hello in one word."}]}')

echo "$response" | grep -i x-lumen-model-used
echo "$response" | tail -n 1
