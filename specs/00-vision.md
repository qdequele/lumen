# Vision - why LUMEN exists

## The problem
Existing LLM gateways have documented structural flaws:

**LiteLLM** (Python): 1.7-4x throughput overhead measured by users in production; DB in the request path (1M logs → slowed-down API); 4 Gi RAM/worker recommended + worker recycling to contain leaks; no cancellation propagation (the GPU keeps generating after the client disconnects); readiness probes that fail under load → cascades of k8s restarts.

**OpenRouter** (SaaS): not self-hostable; outages of their own infrastructure with misleading 401s; 5.5% fees; prompt sampling by default; model IDs that change and break integrations; no hard budget in the request path (agents drain the credits).

**All of them**: reranking and embeddings are second-class citizens, even though every RAG stack needs them.

## The answer
A gateway written in Rust: single binary, chat + embeddings + rerank as equals, < 1 ms overhead, zero telemetry, atomic hard budgets, end-to-end cancellation, DB off the critical path.

## Target user
The dev/team who self-hosts, mixes cloud APIs (OpenAI, Anthropic, Cohere...) and local models (Ollama, vLLM, TEI), builds RAG or agents, and wants reliable production without operating Postgres+Redis+Gunicorn tuning.

## Decision principles (when the spec is silent)
1. When in doubt: the simplest solution that preserves the 4 pillars (performance, sovereignty, robustness, multi-capability).
2. A feature that adds latency to the request path must be opt-in.
3. OpenAI compatibility takes precedence over internal elegance - existing clients must work without modification.
4. Any user data (prompts, documents) is radioactive: never store it, log it, or transmit it anywhere other than to the chosen provider.
