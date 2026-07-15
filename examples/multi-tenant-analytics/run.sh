#!/usr/bin/env bash
set -euo pipefail
BASE_URL="${BASE_URL:-http://localhost:8080}"
: "${LUMEN_MASTER_KEY:?LUMEN_MASTER_KEY must be set - see README.md}"

echo "== admin: create a virtual key for tenant 'acme' with a small budget =="
created=$(curl -sf "$BASE_URL/admin/keys" \
  -H "Authorization: Bearer $LUMEN_MASTER_KEY" \
  -H 'content-type: application/json' \
  -d '{"name": "tenant-acme", "budget_max": 1.0}')
echo "$created"
echo

# The plaintext virtual key is returned exactly once, in this response's
# "key" field (see docs/operations/keys-budgets.md) - extract it without a
# jq dependency.
virtual_key=$(printf '%s' "$created" | grep -o '"key":"[^"]*"' | head -1 | cut -d'"' -f4)
: "${virtual_key:?failed to parse created key from admin response}"

echo "== chat #1 as tenant 'acme', tagged via x-lumen-metadata =="
curl -sf "$BASE_URL/v1/chat/completions" \
  -H "Authorization: Bearer $virtual_key" \
  -H 'content-type: application/json' \
  -H 'x-lumen-metadata: {"org_id":"acme","team_id":"search","project_id":"docs"}' \
  -d '{"model":"gpt-4o","messages":[{"role":"user","content":"Say hello in one word."}]}'
echo

echo "== chat #2 as tenant 'acme' =="
curl -sf "$BASE_URL/v1/chat/completions" \
  -H "Authorization: Bearer $virtual_key" \
  -H 'content-type: application/json' \
  -H 'x-lumen-metadata: {"org_id":"acme","team_id":"search","project_id":"docs"}' \
  -d '{"model":"gpt-4o","messages":[{"role":"user","content":"Count to three."}]}'
echo

echo "== metrics: tokens attributed to org_id=acme =="
curl -sf "$BASE_URL/metrics" | grep 'lumen_tokens_total.*org_id="acme"'
