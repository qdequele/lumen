# ROADMAP — LUMEN

> Instruction pour Claude Code : traite les milestones DANS L'ORDRE. Le milestone courant = premier non coché. Lis sa spec dans `specs/milestones/` avant tout code. Coche les cases ici ET dans la spec au fur et à mesure. Ne commence jamais un milestone si le précédent a des tests rouges.

> **Promesse transverse — comptage des tokens (ADR 003) :** CHAQUE requête de CHAQUE capacité (chat/embed/rerank) produit un compte de tokens, jamais zéro par défaut : usage amont si présent, sinon estimation locale marquée `estimated`. Exposé en réponse, en compteurs Prometheus et dans `usage_log` (M5). C'est une raison d'être centrale du projet.

## M1 — Squelette & fondations ✅
- [x] Workspace Cargo 6 crates (core, providers, router, auth, telemetry, server)
- [x] Types et traits de capacités dans core (ChatProvider, EmbeddingProvider, RerankProvider)
- [x] Serveur axum : /health, /metrics (stub), graceful shutdown
- [x] Config figment (TOML + env) + config.example.toml
- [x] Taxonomie d'erreurs LM-XXXX + réponse JSON d'erreur standard
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
- [x] POST /v1/chat/completions non-streaming, format OpenAI
- [x] Streaming SSE zero-copy (passthrough Bytes quand schéma identique)
- [x] Provider Anthropic avec traduction bidirectionnelle (messages, system, tool_use, usage)
- [x] Providers Mistral + Google (Gemini), streaming inclus
- [x] Déconnexion client → abort amont, testé
- [x] Gardes de stream : first-token timeout (LM-3011), amont mort sans `[DONE]` (LM-3010), heartbeat `: ping`

Note : l'estimation locale des tokens en streaming (usage amont absent →
`estimated=true`, ADR 003) part en M5 avec les compteurs Prometheus et
`usage_log` ; l'usage amont, lui, est déjà propagé (dernier chunk).
Spec : `specs/milestones/M4-chat-streaming.md`

## M5 — Auth, clés virtuelles & budgets durs ✅
- [x] SQLite (sqlx) : clés virtuelles hashées, clés providers chiffrées au repos (AES-GCM)
- [x] Budgets DURS par clé, enforced DANS le chemin de requête avant l'appel amont
- [x] Quotas RPM/TPM par clé
- [x] Comptage coûts par capacité (tokens chat, tokens input embeddings, searches rerank)
- [x] Écriture des logs d'usage via channel borné → writer batché (jamais sync)
- [x] Estimation locale des tokens quand l'amont n'en renvoie pas (streaming inclus), marquée `estimated` (ADR 003)
- [x] Header de métadonnées par requête (`x-lumen-metadata`, style Cloudflare AI Gateway) → logs + `usage_log` + labels Prometheus via allowlist (ADR 002)

Note : l'estimation locale = heuristique octets (inline, hot-path-safe) ;
le tokenizer précis opt-in (spawn_blocking) part en backlog — voir
`docs/backlog.md` § M5.
Spec : `specs/milestones/M5-auth-budgets.md`

## M6 — Résilience ✅
- [x] Retries avec backoff + jitter (honore Retry-After)
- [x] Chaînes de fallback multi-provider par modèle
- [x] Circuit breaker par provider
- [x] Timeouts configurables (connect, first-token, total)
- [x] Health checks providers en tâche de fond, JAMAIS dans le request path
Spec : `specs/milestones/M6-resilience.md`

## M7 — Release ✅
- [x] Benchmarks criterion + comparatif public vs LiteLLM (latence ajoutée, RAM, throughput)
- [x] Dockerfile distroless multi-arch < 20 Mo, binaire statique musl
- [x] Hot reload de config sans drop de connexions
- [x] Docs complètes (README, quickstart, guides providers, errors.md)
- [x] cargo-audit + cargo-deny dans la CI
Spec : `specs/milestones/M7-release.md`

Note : overhead hors réseau mesuré (~3 µs médian, image 10,6 Mo, RSS idle
8,8 Mo) ; le comparatif chargé vs LiteLLM est fourni comme harnais reproductible
(`bench/`) — voir `docs/perf-baseline.md`. cargo-audit/deny/fuzz câblés en CI
(binaires non installés dans l'environnement de dev). Image amd64 via buildx CI ;
arm64 vérifiée localement (`docker run`).

## Backlog v2 (ne pas implémenter)
UI admin, cache sémantique, multimodal (images/audio), guardrails, rate limiting distribué (Redis), OTLP tracing, plugin WASM.
