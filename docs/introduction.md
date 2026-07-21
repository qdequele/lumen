# LUMEN

> **L**ightweight **U**nified **M**odel **EN**dpoint

A universal, self-hostable LLM gateway written in Rust. One OpenAI-compatible
endpoint in front of many providers - for **chat**, **embeddings** and
**reranking** alike. It is designed to be light, fast and sovereign: a single
static binary, **zero telemetry**, and prompts that are **never logged by
default**.

## What's here

- **Getting started** - [installation](getting-started/installation.md),
  [quickstart](getting-started/quickstart.md) and
  [configuration basics](getting-started/configuration.md).
- **Chat** - [completions](chat/completions.md), [streaming](chat/streaming.md),
  [vision](chat/vision.md) and [tool calling](chat/tool-calling.md).
- **Embeddings** - [embeddings](embeddings/embeddings.md),
  [batching](embeddings/batching.md) and
  [multimodal embeddings](embeddings/multimodal.md).
- **Reranking** - [reranking](reranking/reranking.md).
- **Operations** - [token accounting & cost](operations/token-accounting.md),
  [metrics & dashboards](operations/metrics.md),
  [usage log & multi-tenant metadata](operations/usage-log.md),
  [keys, quotas & budgets](operations/keys-budgets.md),
  [resilience tuning](operations/resilience.md) and
  [deployment](operations/deployment.md).
- **[Examples](examples.md)** - ready-made scenario configs.
- **Reference** - [providers](providers.md), [error codes](errors.md) and the
  [performance baseline](perf-baseline.md).
- **[Architecture decisions](adr/001-crate-and-lib-naming.md)** - the ADRs that pin the design.
- **Project** - [backlog](backlog.md) and [contributing](contributing.md).

## The five pillars

Every trade-off is decided in this order:

1. **Performance** - < 1 ms added latency p99, zero-copy streaming, ~15 MB RAM idle.
2. **Sovereignty** - zero telemetry, prompts never logged by default, single binary.
3. **Robustness** - propagated cancellation, backpressure, DB off the request path.
4. **Multi-capability** - chat + embeddings + rerank are first-class citizens.
5. **Token observability** - every request of every capability produces a token
   count: upstream usage when reported, otherwise a local estimate flagged
   `estimated`. Never a silent zero. See
   [token accounting & cost](operations/token-accounting.md).

Want to contribute? Start with the
[contribution guide](https://github.com/qdequele/lumen/blob/main/CONTRIBUTING.md).
