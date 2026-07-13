# M1 — Squelette & fondations

## Objectif
Un binaire qui démarre, sert /health et /metrics, charge sa config, avec toute la structure de crates et la CI en place. Aucun provider encore.

## Tâches

### 1.1 Workspace
- [x] `Cargo.toml` workspace avec crates : `core`, `providers`, `router`, `auth`, `telemetry`, `server`
- [x] Profil release : `lto = "thin"`, `codegen-units = 1`, `panic = "abort"`, `strip = true`
- [x] `rust-toolchain.toml` (stable), `.gitignore`, licence Apache-2.0

### 1.2 crates/core
- [x] Types requêtes/réponses : `ChatRequest/Response/Chunk`, `EmbedRequest/Response`, `RerankRequest/Response` (serde, champs du format OpenAI + Cohere rerank)
- [x] Traits `ChatProvider`, `EmbeddingProvider`, `RerankProvider` (signatures du CLAUDE.md, avec `CancellationToken`)
- [x] `ProviderError` (thiserror) : variantes `Upstream { provider, status, retryable }`, `Timeout`, `Cancelled`, `Translation`, `RateLimited { retry_after }`
- [x] `GatewayError` avec code stable `LM-XXXX`, conversion vers réponse JSON : `{"error": {"code": "LM-1001", "message": "...", "type": "invalid_request|upstream_error|internal"}}`
- [x] `Capability` enum : `Chat | Embed | Rerank`

### 1.3 crates/server
- [x] Binaire axum : `GET /health` (répond toujours 200 si le process vit — aucun I/O), `GET /metrics` (registre Prometheus vide pour l'instant)
- [x] Graceful shutdown sur SIGTERM/SIGINT : arrête d'accepter, draine les requêtes en cours (timeout 30 s)
- [x] Middleware tower : request-id, tracing span par requête, limite de taille de body (configurable, défaut 10 Mo)

### 1.4 Config
- [x] figment : `config.toml` + surcharge par env `LUMEN_*`
- [x] Structs : `ServerConfig { host, port, body_limit }`, `ProviderConfig { name, kind, api_key_env, base_url, models: Vec<ModelConfig> }`, `ModelConfig { id, upstream_id, capabilities }`
- [x] Les clés API sont référencées par NOM de variable d'env (`api_key_env = "OPENAI_API_KEY"`), jamais en clair dans le TOML
- [x] Validation au boot : config invalide = exit(1) avec message précis (fichier, champ, raison)
- [x] `config.example.toml` commenté

### 1.5 CI
- [x] `.github/workflows/ci.yml` : fmt --check, clippy -D warnings, test, sur push + PR

## Critères d'acceptation
1. `cargo run -p server -- --config config.example.toml` démarre en < 100 ms et log la liste des modèles chargés (sans les clés).
2. `curl :8080/health` → 200 `{"status":"ok"}` même si aucune env var de clé n'est définie.
3. Config avec un champ inconnu ou un port invalide → exit(1), message d'erreur nommant le champ.
4. Un test capture les logs au boot et vérifie qu'aucune valeur de clé API n'y figure.
5. SIGTERM pendant une requête en cours (test avec une route de test lente) → la requête se termine, puis le process sort avec code 0.
6. CI verte.

## Pièges à éviter
- Ne pas mettre de connexion DB dans /health (leçon LiteLLM #15526 : readiness probes qui échouent sous charge → cascade de restarts).
- Ne pas dériver `Debug` naïvement sur les structs de config contenant des références de secrets.
