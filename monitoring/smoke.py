#!/usr/bin/env python3
"""LUMEN smoke suite.

Exercises every configured provider through the gateway and checks that each
response is well-formed AND carries a token count (ADR 003: never zero by
default). Beyond the basics (chat, chat streaming, embeddings, reranking) it
covers the advanced features:

  * vision: image content parts in /v1/chat/completions (OpenAI forwarded
    verbatim, Anthropic translated to base64 blocks, Gemini to inline_data)
  * function calling: tools request -> tool_calls response -> tool-result
    roundtrip (OpenAI, Anthropic, Mistral, Gemini), plus streamed tool calls
    (OpenAI)
  * multimodal embeddings: mixed text + image batches (Cohere embed-v4),
    with media accounting asserted on /metrics (M9)
  * gateway guards: LM-2003 (image to a text-only model), LM-2004 (remote
    image URL routed to Gemini - the gateway never fetches URLs itself),
    LM-2005 (remote image URL on /v1/embeddings with [image_fetch] disabled)
  * multi-tenant metadata: org_id/team_id/project_id from x-lumen-metadata
    must come back as Prometheus labels (ADR 002)

Providers whose API key is not set are SKIPPED, not failed - fill in
monitoring/.env to enable them.

    ./smoke.py                          # gateway on http://localhost:8080
    BASE_URL=http://host:8080 ./smoke.py

Python 3.9+, standard library only.
"""

import json
import os
import sys
import urllib.error
import urllib.request
from pathlib import Path

BASE_URL = os.environ.get("BASE_URL", "http://localhost:8080")
SCRIPT_DIR = Path(__file__).resolve().parent
METADATA = json.dumps(
    {"scenario": "smoke", "org_id": "acme", "team_id": "qa", "project_id": "smoke-suite"}
)

# A 1x1 red PNG as a data: URI - exercises vision and multimodal embeddings
# (M9) and the lumen_media_total / lumen_media_bytes_total accounting.
TINY_PNG = (
    "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJ"
    "AAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg=="
)
REMOTE_IMAGE_URL = "https://example.com/some-image.png"  # guards must reject BEFORE any fetch

WEATHER_TOOL = {
    "type": "function",
    "function": {
        "name": "get_weather",
        "description": "Get the current weather for a city.",
        "parameters": {
            "type": "object",
            "properties": {"city": {"type": "string", "description": "City name"}},
            "required": ["city"],
        },
    },
}

GREEN, RED, GRAY, RESET = "\033[32m", "\033[31m", "\033[90m", "\033[0m"
passed = failed = skipped = 0
failures: list[str] = []


def report(status: str, name: str, detail: str = "") -> None:
    global passed, failed, skipped
    if status == "PASS":
        passed += 1
        print(f"  {GREEN}✔ PASS{RESET} {name} {GRAY}{detail}{RESET}")
    elif status == "FAIL":
        failed += 1
        failures.append(f"{name}: {detail}")
        print(f"  {RED}✘ FAIL{RESET} {name} {detail}")
    else:
        skipped += 1
        print(f"  {GRAY}- SKIP{RESET} {name} {GRAY}{detail}{RESET}")


def load_env() -> None:
    """Load monitoring/.env so the skip logic matches what the gateway sees."""
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


def http(method: str, path: str, body: dict | None = None, timeout: int = 120):
    """Return (status_code, parsed_json_or_text). Never raises on HTTP errors."""
    req = urllib.request.Request(
        BASE_URL + path,
        data=json.dumps(body).encode() if body is not None else None,
        method=method,
        headers={"content-type": "application/json", "x-lumen-metadata": METADATA},
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            raw = resp.read().decode()
            code = resp.status
    except urllib.error.HTTPError as e:
        raw = e.read().decode()
        code = e.code
    except OSError as e:
        return 0, str(e)
    try:
        return code, json.loads(raw)
    except json.JSONDecodeError:
        return code, raw


def post_stream(path: str, body: dict, timeout: int = 120) -> str:
    """POST with stream=true and return the whole SSE text."""
    req = urllib.request.Request(
        BASE_URL + path,
        data=json.dumps(body).encode(),
        method="POST",
        headers={"content-type": "application/json", "x-lumen-metadata": METADATA},
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            return resp.read().decode()
    except urllib.error.HTTPError as e:
        return e.read().decode()
    except OSError as e:
        return str(e)


def err_msg(body) -> str:
    if isinstance(body, dict):
        return str(body.get("error", {}).get("message", body))[:160]
    return str(body)[:160]


def err_code(body) -> str:
    if isinstance(body, dict):
        return str(body.get("error", {}).get("code", ""))
    return ""


# --------------------------------------------------------------------------
# Basic per-capability checks
# --------------------------------------------------------------------------

def check_chat(name: str, model: str) -> None:
    code, body = http("POST", "/v1/chat/completions", {
        "model": model, "max_tokens": 512,
        "messages": [{"role": "user", "content": "Reply with exactly: pong"}],
    })
    if code != 200:
        report("FAIL", f"{name} chat", f"HTTP {code} {err_msg(body)}")
        return
    content = body["choices"][0]["message"].get("content") or ""
    tokens = body.get("usage", {}).get("total_tokens", 0)
    if content and tokens > 0:
        report("PASS", f"{name} chat", f"({tokens} tokens)")
    else:
        report("FAIL", f"{name} chat", f"empty content or zero usage: {json.dumps(body)[:160]}")


def check_chat_stream(name: str, model: str) -> None:
    out = post_stream("/v1/chat/completions", {
        "model": model, "max_tokens": 512, "stream": True,
        "stream_options": {"include_usage": True},
        "messages": [{"role": "user", "content": "Count from 1 to 5."}],
    })
    if "data: [DONE]" in out and '"delta"' in out:
        report("PASS", f"{name} chat-stream", "(SSE frames + [DONE])")
    else:
        report("FAIL", f"{name} chat-stream", out[:160])


def check_embed(name: str, model: str) -> None:
    code, body = http("POST", "/v1/embeddings", {
        "model": model,
        "input": ["the fast brown fox", "meilisearch is a search engine"],
    })
    if code != 200:
        report("FAIL", f"{name} embed", f"HTTP {code} {err_msg(body)}")
        return
    data = body.get("data", [])
    dims = len(data[0].get("embedding", [])) if data else 0
    usage = body.get("usage", {})
    tokens = usage.get("total_tokens") or usage.get("prompt_tokens") or 0
    if len(data) == 2 and dims > 0 and tokens > 0:
        report("PASS", f"{name} embed", f"(2 vectors x {dims}d, {tokens} tokens)")
    else:
        report("FAIL", f"{name} embed", f"count={len(data)} dims={dims} tokens={tokens}")


def check_rerank(name: str, model: str) -> None:
    code, body = http("POST", "/v1/rerank", {
        "model": model, "query": "what is a llm gateway", "top_n": 2,
        "documents": [
            "A gateway routes requests to many model providers.",
            "Bananas are rich in potassium.",
            "LUMEN is a self-hostable LLM gateway written in Rust.",
            "The Eiffel Tower is in Paris.",
        ],
    })
    if code != 200:
        report("FAIL", f"{name} rerank", f"HTTP {code} {err_msg(body)}")
        return
    results = body.get("results", [])
    top = results[0].get("index") if results else None
    if len(results) == 2 and top in (0, 2):
        report("PASS", f"{name} rerank", f"(top hit doc #{top})")
    else:
        report("FAIL", f"{name} rerank",
               f"results={len(results)} top_index={top} (expected a gateway doc first)")


# --------------------------------------------------------------------------
# Advanced: vision (media in chat)
# --------------------------------------------------------------------------

def vision_messages(url: str) -> list:
    return [{
        "role": "user",
        "content": [
            {"type": "text", "text": "What colour is this image? One word."},
            {"type": "image_url", "image_url": {"url": url}},
        ],
    }]


def check_chat_vision(name: str, model: str) -> None:
    code, body = http("POST", "/v1/chat/completions",
                      {"model": model, "max_tokens": 512, "messages": vision_messages(TINY_PNG)})
    if code != 200:
        report("FAIL", f"{name} chat-vision", f"HTTP {code} {err_msg(body)}")
        return
    content = (body["choices"][0]["message"].get("content") or "").strip()
    tokens = body.get("usage", {}).get("total_tokens", 0)
    if content and tokens > 0:
        colour = "red" if "red" in content.lower() else content.split()[0]
        report("PASS", f"{name} chat-vision", f'(sees "{colour}", {tokens} tokens)')
    else:
        report("FAIL", f"{name} chat-vision",
               f"empty content or zero usage: {json.dumps(body)[:160]}")


def check_vision_guard_lm2003() -> None:
    """An image part sent to a model without the image modality -> 400 LM-2003."""
    code, body = http("POST", "/v1/chat/completions",
                      {"model": "mistral-small", "max_tokens": 64,
                       "messages": vision_messages(TINY_PNG)})
    if code == 400 and err_code(body) == "LM-2003":
        report("PASS", "guard image-to-text-model", "(400 LM-2003, rejected before upstream)")
    else:
        report("FAIL", "guard image-to-text-model",
               f"expected 400 LM-2003, got HTTP {code} {err_code(body)} {err_msg(body)}")


def check_vision_guard_lm2004() -> None:
    """A remote image URL routed to Gemini -> 400 LM-2004 (never-fetch rule)."""
    code, body = http("POST", "/v1/chat/completions",
                      {"model": "gemini-flash", "max_tokens": 64,
                       "messages": vision_messages(REMOTE_IMAGE_URL)})
    if code == 400 and err_code(body) == "LM-2004":
        report("PASS", "guard remote-url-to-gemini", "(400 LM-2004, gateway never fetches URLs)")
    else:
        report("FAIL", "guard remote-url-to-gemini",
               f"expected 400 LM-2004, got HTTP {code} {err_code(body)} {err_msg(body)}")


# --------------------------------------------------------------------------
# Advanced: function calling
# --------------------------------------------------------------------------

def check_chat_tools(name: str, model: str) -> None:
    """Two-leg tool roundtrip: model asks for the tool, then uses its result."""
    messages = [{"role": "user",
                 "content": "What is the weather in Paris right now? Use the get_weather tool."}]
    code, body = http("POST", "/v1/chat/completions", {
        "model": model, "max_tokens": 512, "messages": messages,
        "tools": [WEATHER_TOOL], "tool_choice": "auto",
    })
    if code != 200:
        report("FAIL", f"{name} chat-tools", f"HTTP {code} {err_msg(body)}")
        return
    choice = body["choices"][0]
    calls = choice["message"].get("tool_calls") or []
    fn = calls[0]["function"] if calls else {}
    args_ok = False
    try:
        args_ok = "paris" in json.dumps(json.loads(fn.get("arguments", "{}"))).lower()
    except json.JSONDecodeError:
        pass
    if not (calls and fn.get("name") == "get_weather"
            and choice.get("finish_reason") == "tool_calls" and args_ok):
        report("FAIL", f"{name} chat-tools",
               f"expected a get_weather(Paris) tool call, got: {json.dumps(choice)[:200]}")
        return

    # Leg 2: return the tool result and expect a final grounded answer.
    messages.append(choice["message"])
    messages.append({"role": "tool", "tool_call_id": calls[0]["id"],
                     "content": '{"temp_c": 21, "condition": "sunny"}'})
    code2, body2 = http("POST", "/v1/chat/completions",
                        {"model": model, "max_tokens": 512, "messages": messages,
                         "tools": [WEATHER_TOOL]})
    if code2 != 200:
        report("FAIL", f"{name} chat-tools", f"roundtrip leg HTTP {code2} {err_msg(body2)}")
        return
    answer = (body2["choices"][0]["message"].get("content") or "").lower()
    if "21" in answer or "sunny" in answer:
        report("PASS", f"{name} chat-tools", "(tool call + result roundtrip, answer grounded)")
    else:
        report("FAIL", f"{name} chat-tools", f"final answer ignores tool result: {answer[:120]}")


def check_chat_tools_stream(name: str, model: str) -> None:
    out = post_stream("/v1/chat/completions", {
        "model": model, "max_tokens": 512, "stream": True,
        "messages": [{"role": "user",
                      "content": "What is the weather in Tokyo? Use the get_weather tool."}],
        "tools": [WEATHER_TOOL], "tool_choice": "auto",
    })
    if "data: [DONE]" in out and '"tool_calls"' in out and "get_weather" in out:
        report("PASS", f"{name} chat-tools-stream", "(streamed tool_calls deltas + [DONE])")
    else:
        report("FAIL", f"{name} chat-tools-stream", out[:160])


# --------------------------------------------------------------------------
# Advanced: multimodal embeddings (M9)
# --------------------------------------------------------------------------

def check_embed_image(name: str, model: str) -> None:
    """Mixed batch: one plain-text item + one text+image content-parts item."""
    code, body = http("POST", "/v1/embeddings", {
        "model": model,
        "input": [
            "a plain text sentence",
            [{"type": "text", "text": "a red pixel"},
             {"type": "image_url", "image_url": {"url": TINY_PNG}}],
        ],
    })
    if code != 200:
        report("FAIL", f"{name} embed-image", f"HTTP {code} {err_msg(body)}")
        return
    data = body.get("data", [])
    dims = {len(item.get("embedding", [])) for item in data}
    if len(data) == 2 and dims and 0 not in dims:
        report("PASS", f"{name} embed-image",
               f"(mixed text+image batch, 2 vectors x {dims.pop()}d)")
    else:
        report("FAIL", f"{name} embed-image", f"count={len(data)} dims={dims}")


def check_embed_guard_lm2005(model: str) -> None:
    """Remote image URL on /v1/embeddings with [image_fetch] disabled -> LM-2005."""
    code, body = http("POST", "/v1/embeddings", {
        "model": model,
        "input": [[{"type": "image_url", "image_url": {"url": REMOTE_IMAGE_URL}}]],
    })
    if code == 400 and err_code(body) == "LM-2005":
        report("PASS", "guard remote-url-embed", "(400 LM-2005, image_fetch disabled)")
    else:
        report("FAIL", "guard remote-url-embed",
               f"expected 400 LM-2005, got HTTP {code} {err_code(body)} {err_msg(body)}")


def check_latency_metrics() -> None:
    """Every endpoint hit above must have latency histogram samples."""
    code, text = http("GET", "/metrics")
    lines = str(text).splitlines()
    http_ok = any(l.startswith("lumen_http_request_duration_seconds_bucket{")
                  and 'path="/v1/chat/completions"' in l for l in lines)
    e2e_ok = any(l.startswith("lumen_request_duration_seconds_bucket{")
                 and 'provider="' in l for l in lines)
    if code == 200 and http_ok and e2e_ok:
        report("PASS", "latency metrics",
               "(http_request_duration by route + request_duration by provider/model)")
    else:
        report("FAIL", "latency metrics",
               f"histograms missing on /metrics (route={http_ok}, e2e={e2e_ok})")


def check_media_accounting() -> None:
    """The image embeds above must show up in the M9 media counters."""
    code, text = http("GET", "/metrics")
    lines = [l for l in str(text).splitlines()
             if l.startswith("lumen_media_total{") and 'media_type="image"' in l]
    if code == 200 and any('capability="embed"' in l for l in lines):
        report("PASS", "media accounting", "(lumen_media_total{capability=embed,media_type=image})")
    else:
        report("FAIL", "media accounting", "no image series on lumen_media_total after image embed")


# --------------------------------------------------------------------------
# Suite
# --------------------------------------------------------------------------

def provider(env_var: str, name: str, tests: list) -> bool:
    """Print the section header; SKIP every listed test if the key is absent."""
    print(f"\n== {name}")
    if os.environ.get(env_var):
        return True
    for test_name in tests:
        report("SKIP", test_name, f"({env_var} not set)")
    return False


def main() -> int:
    load_env()
    print(f"LUMEN smoke suite -> {BASE_URL}")

    print("\n== gateway")
    code, body = http("GET", "/health", timeout=5)
    if code == 200:
        report("PASS", "health", f"({json.dumps(body)})")
    else:
        report("FAIL", "health", f"no healthy response from {BASE_URL}/health (HTTP {code})")

    code, body = http("GET", "/v1/models", timeout=5)
    count = len(body.get("data", [])) if isinstance(body, dict) else 0
    if count >= 12:
        report("PASS", "models", f"({count} models listed)")
    else:
        report("FAIL", "models", f"expected >= 12 models, got {count}")

    code, text = http("GET", "/metrics", timeout=5)
    if code == 200 and "lumen_" in str(text):
        report("PASS", "metrics", "(lumen_* series exposed)")
    elif code == 200:
        report("PASS", "metrics", "(reachable; series appear after first request)")
    else:
        report("FAIL", "metrics", f"{BASE_URL}/metrics unreachable (HTTP {code})")

    if provider("OPENAI_API_KEY", "openai",
                ["openai chat", "openai chat-stream", "openai chat-vision",
                 "openai chat-tools", "openai chat-tools-stream", "openai embed"]):
        check_chat("openai", "gpt-4o-mini")
        check_chat_stream("openai", "gpt-4o-mini")
        check_chat_vision("openai", "gpt-4o-mini")
        check_chat_tools("openai", "gpt-4o-mini")
        check_chat_tools_stream("openai", "gpt-4o-mini")
        check_embed("openai", "text-embedding-3-small")

    if provider("ANTHROPIC_API_KEY", "anthropic",
                ["anthropic chat", "anthropic chat-stream", "anthropic chat-vision",
                 "anthropic chat-tools"]):
        check_chat("anthropic", "claude-haiku-4-5")
        check_chat_stream("anthropic", "claude-haiku-4-5")
        check_chat_vision("anthropic", "claude-haiku-4-5")
        check_chat_tools("anthropic", "claude-haiku-4-5")

    if provider("MISTRAL_API_KEY", "mistral",
                ["mistral chat", "mistral chat-stream", "mistral chat-tools", "mistral embed"]):
        check_chat("mistral", "mistral-small")
        check_chat_stream("mistral", "mistral-small")
        check_chat_tools("mistral", "mistral-small")
        check_embed("mistral", "mistral-embed")

    if provider("GEMINI_API_KEY", "google (gemini)",
                ["gemini chat", "gemini chat-stream", "gemini chat-vision",
                 "gemini chat-tools"]):
        check_chat("gemini", "gemini-flash")
        check_chat_stream("gemini", "gemini-flash")
        check_chat_vision("gemini", "gemini-flash")
        check_chat_tools("gemini", "gemini-flash")

    if provider("COHERE_API_KEY", "cohere",
                ["cohere embed", "cohere embed-image", "cohere rerank"]):
        check_embed("cohere", "cohere-embed")
        check_embed_image("cohere", "cohere-embed")
        check_rerank("cohere", "cohere-rerank")

    if provider("JINA_API_KEY", "jina", ["jina embed", "jina rerank"]):
        check_embed("jina", "jina-embed")
        check_rerank("jina", "jina-rerank")

    if provider("VOYAGE_API_KEY", "voyage", ["voyage embed", "voyage rerank"]):
        check_embed("voyage", "voyage-embed")
        check_rerank("voyage", "voyage-rerank")

    cf_ready = provider("CLOUDFLARE_API_TOKEN", "cloudflare workers ai",
                        ["cloudflare chat", "cloudflare embed"])
    if cf_ready:
        if "YOUR_CLOUDFLARE_ACCOUNT_ID" in (SCRIPT_DIR / "lumen.toml").read_text():
            report("SKIP", "cloudflare chat", "(account id not set in lumen.toml)")
            report("SKIP", "cloudflare embed", "(account id not set in lumen.toml)")
        else:
            check_chat("cloudflare", "cf-llama")
            check_embed("cloudflare", "cf-embed")

    # Gateway guards: pure pre-flight rejections, no provider key is consumed,
    # but each needs its model configured (they all are, in lumen.toml).
    print("\n== gateway guards (pre-flight rejections)")
    check_vision_guard_lm2003()
    check_vision_guard_lm2004()
    check_embed_guard_lm2005("cohere-embed")
    if os.environ.get("COHERE_API_KEY"):
        check_media_accounting()
    else:
        report("SKIP", "media accounting", "(needs a successful cohere image embed)")
    if passed > 3:
        check_latency_metrics()
    else:
        report("SKIP", "latency metrics", "(no successful API call to measure)")

    # Multi-tenant metadata (ADR 002): the allowlisted keys must come back as
    # Prometheus labels on the token counters.
    print("\n== multi-tenant metadata")
    if passed > 3:
        _, text = http("GET", "/metrics", timeout=5)
        token_lines = [l for l in str(text).splitlines() if l.startswith("lumen_tokens_total{")]
        wanted = ('org_id="acme"', 'team_id="qa"', 'project_id="smoke-suite"')
        if any(all(w in l for w in wanted) for l in token_lines):
            report("PASS", "tenant labels", "(org_id/team_id/project_id on lumen_tokens_total)")
        else:
            report("FAIL", "tenant labels",
                   "org_id/team_id/project_id missing - is telemetry.metadata_labels set?")
    else:
        report("SKIP", "tenant labels", "(no provider call succeeded to carry metadata)")

    print("\n" + "=" * 64)
    print(f"PASS {passed}   FAIL {failed}   SKIP {skipped}")
    if failed:
        print("Failures:")
        for f in failures:
            print(f"  - {f}")
        return 1
    print("Grafana: http://localhost:3000/d/lumen-gateway (admin / lumen)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
