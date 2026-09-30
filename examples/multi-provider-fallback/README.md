# multi-provider-fallback

Cross-vendor chat fallback: the `gpt-4o` virtual model serves from OpenAI
(`openai/gpt-4o`) first, then from `claude-sonnet-4-5` (Anthropic), as its
`fallback` targets. See [Virtual models](../../docs/virtual-models.md). Needs
`OPENAI_API_KEY` and `ANTHROPIC_API_KEY`; the Anthropic key is only used if
the fallback actually fires.

## What it shows

Whichever model actually served a request (primary or fallback) is reported
in the `x-lumen-model-used` response header, so a caller can tell when a
fallback fired without inspecting the response body. See the book's
[Resilience tuning](../../docs/operations/resilience.md) for the full retry,
fallback and circuit breaker behavior.

## Run it

```bash
# terminal 1 - start the gateway with this scenario's config
export OPENAI_API_KEY=sk-...
export ANTHROPIC_API_KEY=sk-ant-...
cargo run -p server -- --config examples/multi-provider-fallback/config.toml

# terminal 2 - fire the request
./examples/multi-provider-fallback/run.sh
```

With both keys valid and OpenAI healthy, `x-lumen-model-used` reads
`openai/gpt-4o` on every call, and `x-lumen-route` reads
`gpt-4o>openai/gpt-4o`.

## Watching the fallback fire

1. Start the gateway with `OPENAI_API_KEY` set to a bogus value (so every
   call to OpenAI fails) and a valid `ANTHROPIC_API_KEY`.
2. Send 5 or more requests with `./run.sh` (or repeat the curl in it). The
   circuit breaker's default `circuit_failure_threshold` is 5 consecutive
   failures, so after the 5th failing call to OpenAI the breaker trips open
   for that provider.
3. Once the breaker is open, `x-lumen-model-used` flips from `openai/gpt-4o` to
   `claude-sonnet-4-5` on subsequent calls: the router stops even trying the
   broken primary and goes straight to the fallback.
