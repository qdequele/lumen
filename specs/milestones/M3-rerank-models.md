# M3 - Reranking + model discovery

## Objective
Rerank (Cohere format) and `/v1/models` with capabilities. LUMEN becomes the only gateway where chat/embed/rerank are equals.

## Tasks

### 3.1 Rerank endpoint
- [x] `POST /v1/rerank`: body `{ model, query, documents: [string|{text}], top_n?, return_documents? }`
- [x] Response: `{ results: [{index, relevance_score, document?}], usage: {search_units} }`
- [x] Validation: empty documents → 400 LM-2010; top_n > len(documents) → silent clamp

### 3.2 Providers
- [x] Cohere: `EmbeddingProvider` + `RerankProvider` (API v2)
- [x] Jina: `EmbeddingProvider` + `RerankProvider`
- [x] TEI (Text Embeddings Inference, self-hosted): `EmbeddingProvider` + `RerankProvider` - `/embed` and `/rerank` API, no auth by default
- [x] Voyage: `EmbeddingProvider` + `RerankProvider`
- [x] Each one passes the M2 conformance suite (extended to rerank: `rerank_conformance_suite`)

### 3.3 /v1/models
- [x] `GET /v1/models`: extended OpenAI format - `{ id, object: "model", owned_by: <provider>, capabilities: ["chat"|"embed"|"rerank"] }`
- [x] Reflects ONLY the user's config (no upstream introspection)

### 3.4 Versioned aliasing
- [x] Config: `[[providers.models]] id = "my-embedder" upstream_id = "text-embedding-3-large"` - the public ID belongs to the user
- [x] Multiple aliases can point to the same upstream_id
- [x] ID collision between providers → boot-time error with the two conflicting locations

## Acceptance criteria
1. Rerank conformance suite passed by Cohere, Jina, TEI, Voyage (wiremock).
2. `curl /v1/rerank` with 3 documents → results sorted by descending score, index pointing to the original position.
3. `/v1/models` lists each alias with the right capabilities; a Cohere model configured for embed+rerank appears with both.
4. Boot with two models at the same id → exit(1), message citing the two providers.
5. `return_documents: false` (default) → no document field in the results (bandwidth savings).
