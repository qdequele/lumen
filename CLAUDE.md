# Ferrogate — LLM Gateway universelle en Rust

## Mission
Gateway self-hostable, légère et rapide pour **tous les types de modèles** : chat/LLM, embeddings, reranking. Alternative à LiteLLM (trop lourd, Python, 1.7-4x d'overhead) et OpenRouter (SaaS, pas self-hostable, télémétrie).

## Piliers non négociables (dans l'ordre)
1. **Performance** : < 1 ms de latence ajoutée p99, streaming zero-copy, ~15 Mo RAM idle.
2. **Souveraineté** : zéro télémétrie, prompts JAMAIS loggés par défaut, binaire unique self-host.
3. **Robustesse** : cancellation propagée, backpressure, DB hors du chemin de requête.
4. **Multi-capacités** : chat + embeddings + rerank sont des citoyens de première classe.
5. **Observabilité des tokens** : CHAQUE requête de CHAQUE capacité produit un compte de tokens (jamais zéro par défaut) — usage amont si dispo, sinon estimation locale marquée `estimated`. Exposé en réponse, en Prometheus et dans `usage_log`. Raison d'être centrale. Voir ADR 003.

## Architecture (workspace Cargo)
```
crates/
├── core        # types partagés, traits Provider, erreurs (thiserror)
├── providers   # 1 module par provider (openai, anthropic, cohere, ollama, tei...)
├── router      # résolution modèle→provider, fallback, load balancing
├── auth        # clés virtuelles, quotas, budgets durs
├── telemetry   # métriques Prometheus, logs structurés (tracing), comptage TOKENS (ADR 003) + coûts
└── server      # binaire axum, SSE, config, hot reload
```

### Traits de capacités (crates/core)
```rust
#[async_trait]
pub trait ChatProvider: Send + Sync {
    async fn chat(&self, req: ChatRequest, cancel: CancellationToken)
        -> Result<ChatResponse, ProviderError>;
    async fn chat_stream(&self, req: ChatRequest, cancel: CancellationToken)
        -> Result<BoxStream<'static, Result<ChatChunk, ProviderError>>, ProviderError>;
}

#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    async fn embed(&self, req: EmbedRequest, cancel: CancellationToken)
        -> Result<EmbedResponse, ProviderError>;
    fn max_batch_size(&self) -> usize;
}

#[async_trait]
pub trait RerankProvider: Send + Sync {
    async fn rerank(&self, req: RerankRequest, cancel: CancellationToken)
        -> Result<RerankResponse, ProviderError>;
}
```
Un provider implémente 1 à N traits. Le router route par (capacité, modèle).

### API publique
- `POST /v1/chat/completions` — format OpenAI, streaming SSE
- `POST /v1/embeddings` — format OpenAI
- `POST /v1/rerank` — format Cohere (`query`, `documents`, `top_n`)
- `GET /v1/models` — expose `"capabilities": ["chat"|"embed"|"rerank"]` par modèle
- `GET /health` — chemin isolé, ne touche NI la DB NI les providers
- `GET /metrics` — Prometheus

## Stack imposée
- **Runtime** : tokio (multi-thread), axum, tower, hyper
- **HTTP client** : reqwest (rustls, PAS openssl)
- **Sérialisation** : serde + serde_json
- **DB** : sqlx + SQLite par défaut ; Postgres derrière feature flag `postgres`
- **Erreurs** : thiserror dans les libs, anyhow SEULEMENT dans main.rs
- **Logs** : tracing + tracing-subscriber (JSON en prod)
- **Config** : figment (TOML + env vars), hot reload via notify
- **Tests** : wiremock pour mocker les providers, tokio::test

## Règles de code STRICTES
1. **INTERDIT** : `unwrap()`, `expect()`, `panic!()` hors tests et main.rs (justifier avec un commentaire si exception).
2. **INTERDIT** : bloquer le runtime tokio (pas de `std::thread::sleep`, pas d'I/O sync).
3. **OBLIGATOIRE** : tout appel provider prend un `CancellationToken` ; drop du client HTTP = abort de la requête amont (leçon LiteLLM issue #22805).
4. **OBLIGATOIRE** : le logging des requêtes passe par un channel mpsc borné → writer batché async. JAMAIS d'écriture DB synchrone dans le chemin de requête (leçon LiteLLM issue #12067).
5. **OBLIGATOIRE** : les secrets providers ne sont JAMAIS loggés, jamais dans les erreurs retournées au client, jamais en Debug (`#[derive]` custom ou `secrecy` crate).
6. Clippy pedantic activé : `cargo clippy --workspace --all-targets -- -D warnings` doit passer.
7. Chaque module public a un doc comment. Chaque erreur a un code stable (`FG-1001` etc.) documenté dans `docs/errors.md`.
8. Les erreurs distinguent TOUJOURS : erreur client (4xx) / erreur provider amont (502/503 + nom du provider) / erreur gateway interne (500). Jamais de 401 trompeur pendant une panne interne (leçon OpenRouter).

## Boucle de travail (à suivre à chaque session)
0. **Fraîcheur des dépendances (TOUJOURS, en début de session)** : `rustup update`
   pour la dernière stable, puis `cargo outdated --workspace --root-deps-only`.
   Monter les versions dans `Cargo.toml` quand c'est sûr, puis relancer la
   validation (étape 4) — clippy pedantic peut introduire de nouveaux lints à
   chaque version de Rust. Noter tout bump notable dans `CHANGELOG.md`.
1. Lire `ROADMAP.md` → identifier le milestone courant (premier non coché).
2. Lire `specs/milestones/M<N>-*.md` correspondant.
3. Pour chaque tâche du milestone : écrire les tests D'ABORD, puis l'implémentation.
4. Valider : `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --check`.
5. Cocher les cases dans ROADMAP.md et le fichier de milestone. Ajouter une entrée dans `CHANGELOG.md`.
6. Commit atomique par tâche : `feat(router): fallback chain with circuit breaker`.
7. Si un choix d'architecture n'est pas couvert par les specs : écrire une ADR courte dans `docs/adr/NNN-titre.md` AVANT d'implémenter.

## Definition of Done (par tâche)
- [ ] Tests unitaires + au moins 1 test d'intégration (wiremock)
- [ ] Aucun warning clippy
- [ ] Cancellation testée si la tâche touche le chemin de requête
- [ ] Pas de secret dans les logs (vérifier avec un test)
- [ ] Doc comments sur l'API publique

## Commandes
```bash
cargo test --workspace                # tests
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
cargo run -p server -- --config config.example.toml
cargo bench                           # benchmarks criterion (M7)
```

## Subagents disponibles (.claude/agents/)
- `provider-integrator` : ajoute un nouveau provider (pattern répétable)
- `test-writer` : écrit les tests avant l'implémentation
- `code-reviewer` : review read-only après chaque milestone
- `perf-auditor` : traque allocations, copies, blocages du runtime
- `docs-writer` : docs utilisateur, README, exemples de config

## Ce qu'on ne fait PAS (v1)
UI web, billing, cache sémantique, guardrails/moderation, support des images/audio, plugin system. Noter les idées dans `docs/backlog.md` et passer.
