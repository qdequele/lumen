# M3 — Reranking + découverte de modèles

## Objectif
Le rerank (format Cohere) et `/v1/models` avec capacités. Ferrogate devient la seule gateway où chat/embed/rerank sont égaux.

## Tâches

### 3.1 Endpoint rerank
- [ ] `POST /v1/rerank` : body `{ model, query, documents: [string|{text}], top_n?, return_documents? }`
- [ ] Réponse : `{ results: [{index, relevance_score, document?}], usage: {search_units} }`
- [ ] Validation : documents vide → 400 FG-2010 ; top_n > len(documents) → clamp silencieux

### 3.2 Providers
- [ ] Cohere : `EmbeddingProvider` + `RerankProvider` (API v2)
- [ ] Jina : `EmbeddingProvider` + `RerankProvider`
- [ ] TEI (Text Embeddings Inference, self-hosted) : `EmbeddingProvider` + `RerankProvider` — API `/embed` et `/rerank`, pas d'auth par défaut
- [ ] Voyage : `EmbeddingProvider` + `RerankProvider`
- [ ] Chacun passe la suite de conformité de M2 (étendue au rerank : `rerank_conformance_suite`)

### 3.3 /v1/models
- [ ] `GET /v1/models` : format OpenAI étendu — `{ id, object: "model", owned_by: <provider>, capabilities: ["chat"|"embed"|"rerank"] }`
- [ ] Reflète UNIQUEMENT la config utilisateur (pas d'introspection amont)

### 3.4 Aliasing versionné
- [ ] Config : `[[providers.models]] id = "my-embedder" upstream_id = "text-embedding-3-large"` — l'ID public appartient à l'utilisateur
- [ ] Plusieurs alias peuvent pointer vers le même upstream_id
- [ ] Collision d'ID entre providers → erreur au boot avec les deux emplacements en conflit

## Critères d'acceptation
1. Suite de conformité rerank passée par Cohere, Jina, TEI, Voyage (wiremock).
2. `curl /v1/rerank` avec 3 documents → results triés par score décroissant, index pointant vers la position d'origine.
3. `/v1/models` liste chaque alias avec les bonnes capacités ; un modèle Cohere configuré embed+rerank apparaît avec les deux.
4. Boot avec deux modèles au même id → exit(1), message citant les deux providers.
5. `return_documents: false` (défaut) → pas de champ document dans les results (économie de bande passante).
