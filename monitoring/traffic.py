#!/usr/bin/env python3
"""Continuous traffic generator for the LUMEN monitoring rig.

Fires a random request at the gateway every couple of seconds across every
provider that has a key: chat, streamed chat, vision chat, tool-calling chat,
text embeddings, image embeddings, reranking. Each request is attributed to a
random simulated tenant via x-lumen-metadata, so the Grafana dashboard's
multi-tenant panels have real consumption to show.

    ./traffic.py              # run for 10 minutes
    ./traffic.py 3600         # run for an hour
    INTERVAL=1 ./traffic.py   # faster cadence (seconds between requests)

Stop anytime with Ctrl-C. Python 3.9+, standard library only.
"""

import json
import os
import random
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

BASE_URL = os.environ.get("BASE_URL", "http://localhost:8080")
DURATION = int(sys.argv[1]) if len(sys.argv) > 1 else 600
INTERVAL = float(os.environ.get("INTERVAL", "2"))
SCRIPT_DIR = Path(__file__).resolve().parent

# Simulated tenants (ADR 002): keep the set BOUNDED - every org/team/project
# combination becomes a Prometheus time series.
TENANTS = [
    {"org_id": "acme", "team_id": "search", "project_id": "website-search"},
    {"org_id": "acme", "team_id": "search", "project_id": "catalog-search"},
    {"org_id": "acme", "team_id": "rag", "project_id": "docs-chat"},
    {"org_id": "globex", "team_id": "ml", "project_id": "support-bot"},
    {"org_id": "globex", "team_id": "ml", "project_id": "ticket-triage"},
    {"org_id": "initech", "team_id": "platform", "project_id": "internal-copilot"},
]

PROMPTS = [
    "Give me one fun fact about Rust (the language).",
    "Summarise what an LLM gateway does in one sentence.",
    "Name three European capitals.",
    "Write a haiku about observability.",
    "What is 17 * 23? Answer with the number only.",
]
TEXTS = [
    "Meilisearch is a lightning fast search engine.",
    "Prometheus scrapes metrics over HTTP.",
    "Grafana renders time series beautifully.",
    "Tokio is an async runtime for Rust.",
    "Reranking reorders documents by relevance.",
]

TINY_PNG = (
    "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJ"
    "AAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg=="
)

WEATHER_TOOL = {
    "type": "function",
    "function": {
        "name": "get_weather",
        "description": "Get the current weather for a city.",
        "parameters": {
            "type": "object",
            "properties": {"city": {"type": "string"}},
            "required": ["city"],
        },
    },
}


def load_env() -> None:
    env_file = SCRIPT_DIR / ".env"
    if not env_file.is_file():
        return
    for line in env_file.read_text().splitlines():
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        key, _, value = line.partition("=")
        if value:
            os.environ.setdefault(key.strip(), value.strip())


def build_pool() -> list:
    """(kind, model) pairs for every provider whose key is present."""
    pool: list[tuple[str, str]] = []
    if os.environ.get("OPENAI_API_KEY"):
        pool += [("chat", "gpt-4o-mini"), ("chat-vision", "gpt-4o-mini"),
                 ("chat-tools", "gpt-4o-mini"), ("embed", "text-embedding-3-small")]
    if os.environ.get("ANTHROPIC_API_KEY"):
        pool += [("chat", "claude-haiku-4-5"), ("chat-vision", "claude-haiku-4-5"),
                 ("chat-tools", "claude-haiku-4-5")]
    if os.environ.get("MISTRAL_API_KEY"):
        pool += [("chat", "mistral-small"), ("chat-tools", "mistral-small"),
                 ("embed", "mistral-embed")]
    if os.environ.get("GEMINI_API_KEY"):
        pool += [("chat", "gemini-flash"), ("chat-vision", "gemini-flash")]
    if os.environ.get("COHERE_API_KEY"):
        pool += [("embed", "cohere-embed"), ("embed-image", "cohere-embed"),
                 ("rerank", "cohere-rerank")]
    if os.environ.get("JINA_API_KEY"):
        pool += [("embed", "jina-embed"), ("rerank", "jina-rerank")]
    if os.environ.get("VOYAGE_API_KEY"):
        pool += [("embed", "voyage-embed"), ("rerank", "voyage-rerank")]
    if os.environ.get("CLOUDFLARE_API_TOKEN") and \
            "YOUR_CLOUDFLARE_ACCOUNT_ID" not in (SCRIPT_DIR / "lumen.toml").read_text():
        pool += [("chat", "cf-llama"), ("embed", "cf-embed")]
    return pool


def build_request(kind: str, model: str) -> tuple[str, dict]:
    """Return (path, body) for one randomized request."""
    if kind == "chat":
        return "/v1/chat/completions", {
            "model": model, "max_tokens": 256,
            "messages": [{"role": "user", "content": random.choice(PROMPTS)}],
        }
    if kind == "chat-vision":
        return "/v1/chat/completions", {
            "model": model, "max_tokens": 256,
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "What colour is this image? One word."},
                    {"type": "image_url", "image_url": {"url": TINY_PNG}},
                ],
            }],
        }
    if kind == "chat-tools":
        city = random.choice(["Paris", "Tokyo", "Lima", "Oslo"])
        return "/v1/chat/completions", {
            "model": model, "max_tokens": 256,
            "messages": [{"role": "user",
                          "content": f"What is the weather in {city}? Use the get_weather tool."}],
            "tools": [WEATHER_TOOL], "tool_choice": "auto",
        }
    if kind == "embed":
        return "/v1/embeddings", {
            "model": model,
            "input": random.sample(TEXTS, 2),
        }
    if kind == "embed-image":
        return "/v1/embeddings", {
            "model": model,
            "input": [[{"type": "text", "text": "a red pixel"},
                       {"type": "image_url", "image_url": {"url": TINY_PNG}}]],
        }
    # rerank
    return "/v1/rerank", {
        "model": model, "query": random.choice(PROMPTS), "top_n": 2,
        "documents": TEXTS,
    }


def send(path: str, body: dict, tenant: dict) -> int:
    metadata = json.dumps({**tenant, "scenario": "traffic"})
    req = urllib.request.Request(
        BASE_URL + path,
        data=json.dumps(body).encode(),
        method="POST",
        headers={"content-type": "application/json", "x-lumen-metadata": metadata},
    )
    try:
        with urllib.request.urlopen(req, timeout=60) as resp:
            resp.read()
            return resp.status
    except urllib.error.HTTPError as e:
        return e.code
    except OSError:
        return 0


def main() -> int:
    load_env()
    pool = build_pool()
    if not pool:
        print(f"No provider keys set in {SCRIPT_DIR / '.env'} - nothing to send.",
              file=sys.stderr)
        return 1

    print(f"Traffic -> {BASE_URL} for {DURATION}s, every {INTERVAL}s, "
          f"over {len(pool)} kind/model pairs. Ctrl-C to stop.")

    end = time.monotonic() + DURATION
    sent = errors = 0
    try:
        while time.monotonic() < end:
            kind, model = random.choice(pool)
            path, body = build_request(kind, model)
            code = send(path, body, random.choice(TENANTS))
            sent += 1
            if code != 200:
                errors += 1
            print(f"\r{time.strftime('%H:%M:%S')} sent={sent} errors={errors} "
                  f"last={code} {kind}:{model:<24}", end="", flush=True)
            time.sleep(INTERVAL)
    except KeyboardInterrupt:
        pass
    print(f"\nDone: {sent} requests, {errors} errors. "
          "Dashboard: http://localhost:3000/d/lumen-gateway")
    return 0


if __name__ == "__main__":
    sys.exit(main())
