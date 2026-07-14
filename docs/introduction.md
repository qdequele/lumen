# LUMEN

> **L**ightweight **U**nified **M**odel **EN**dpoint

A universal, self-hostable LLM gateway written in Rust. One OpenAI-compatible
endpoint in front of many providers - for **chat**, **embeddings** and
**reranking** alike. It is designed to be light, fast and sovereign: a single
static binary, **zero telemetry**, and prompts that are **never logged by
default**.

This site is the reference documentation. For a quickstart, the provider
matrix and configuration walkthrough, see the
[README on GitHub](https://github.com/qdequele/lumen#readme).

## What's here

- **[Providers](providers.md)** - the provider × capability matrix and per-provider notes.
- **[Error codes](errors.md)** - the stable `LM-*` error taxonomy returned by the gateway.
- **[Performance baseline](perf-baseline.md)** - measured overhead and the method behind the numbers.
- **[Architecture decisions](adr/001-crate-and-lib-naming.md)** - the ADRs that pin the design.
- **[Backlog](backlog.md)** - ideas and deferred work not in scope for v1.

## The four pillars

Every trade-off is decided in this order:

1. **Performance** - < 1 ms added latency p99, zero-copy streaming, ~15 MB RAM idle.
2. **Sovereignty** - zero telemetry, prompts never logged by default, single binary.
3. **Robustness** - propagated cancellation, backpressure, DB off the request path.
4. **Multi-capability** - chat + embeddings + rerank are first-class citizens.

Want to contribute? Start with the
[contribution guide](https://github.com/qdequele/lumen/blob/main/CONTRIBUTING.md).
