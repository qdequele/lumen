# ROADMAP — Ferrogate

> Instruction pour Claude Code : traite les milestones DANS L'ORDRE. Le milestone courant = premier non coché. Lis sa spec dans `specs/milestones/` avant tout code. Coche les cases ici ET dans la spec au fur et à mesure. Ne commence jamais un milestone si le précédent a des tests rouges.

## M1 — Squelette & fondations ✅
- [x] Workspace Cargo 6 crates (core, providers, router, auth, telemetry, server)
- [x] Types et traits de capacités dans core (ChatProvider, EmbeddingProvider, RerankProvider)
- [x] Serveur axum : /health, /metrics (stub), graceful shutdown
- [x] Config figment (TOML + env) + config.example.toml
- [x] Taxonomie d'erreurs FG-XXXX + réponse JSON d'erreur standard
- [x] CI GitHub Actions : fmt + clippy -D warnings + tests
Spec : `specs/milestones/M1-skeleton.md`

## M2 — Embeddings (premier chemin complet) ✅
- [x] POST /v1/embeddings format OpenAI
- [x] Provider OpenAI embeddings + provider Ollama embeddings
- [x] Batching automatique (découpage selon max_batch_size, réassemblage ordonné)
- [x] Router : résolution (capacité, modèle) → provider depuis la config
- [x] Cancellation de bout en bout testée
Spec : `specs/milestones/M2-embeddings.md`

## M3 — Reranking + découverte de modèles ✅
- [x] POST /v1/rerank format Cohere
- [x] Providers : Cohere (embed+rerank), Jina (embed+rerank), TEI self-hosted (embed+rerank), Voyage (embed+rerank)
- [x] GET /v1/models avec capabilities par modèle
- [x] Aliasing de modèles versionné dans la config (les IDs n'appartiennent qu'à l'utilisateur)
Spec : `specs/milestones/M3-rerank-models.md`

## M4 — Chat + streaming SSE
- [ ] POST /v1/chat/completions non-streaming, format OpenAI
- [ ] Streaming SSE zero-copy (passthrough Bytes quand schéma identique)
- [ ] Provider Anthropic avec traduction bidirectionnelle (messages, system, tool_use, usage)
- [ ] Providers Mistral + Google (Gemini)
- [ ] Déconnexion client → abort amont, testé
Spec : `specs/milestones/M4-chat-streaming.md`

## M5 — Auth, clés virtuelles & budgets durs
- [ ] SQLite (sqlx) : clés virtuelles hashées, clés providers chiffrées au repos (AES-GCM)
- [ ] Budgets DURS par clé, enforced DANS le chemin de requête avant l'appel amont
- [ ] Quotas RPM/TPM par clé
- [ ] Comptage coûts par capacité (tokens chat, tokens input embeddings, searches rerank)
- [ ] Écriture des logs d'usage via channel borné → writer batché (jamais sync)
Spec : `specs/milestones/M5-auth-budgets.md`

## M6 — Résilience
- [ ] Retries avec backoff + jitter (honore Retry-After)
- [ ] Chaînes de fallback multi-provider par modèle
- [ ] Circuit breaker par provider
- [ ] Timeouts configurables (connect, first-token, total)
- [ ] Health checks providers en tâche de fond, JAMAIS dans le request path
Spec : `specs/milestones/M6-resilience.md`

## M7 — Release
- [ ] Benchmarks criterion + comparatif public vs LiteLLM (latence ajoutée, RAM, throughput)
- [ ] Dockerfile distroless multi-arch < 20 Mo, binaire statique musl
- [ ] Hot reload de config sans drop de connexions
- [ ] Docs complètes (README, quickstart, guides providers, errors.md)
- [ ] cargo-audit + cargo-deny dans la CI
Spec : `specs/milestones/M7-release.md`

## Backlog v2 (ne pas implémenter)
UI admin, cache sémantique, multimodal (images/audio), guardrails, rate limiting distribué (Redis), OTLP tracing, plugin WASM.
