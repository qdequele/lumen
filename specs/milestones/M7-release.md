# M7 — Release

## Objectif
Prouver les promesses (benchmarks publics), packager, documenter. Sortie = v0.1.0 taguée.

## Tâches

### 7.1 Benchmarks
- [ ] Criterion : latence ajoutée par la gateway (proxy vs direct, mock amont local), overhead streaming par chunk, throughput embeddings batchés
- [ ] Harnais de comparaison reproductible vs LiteLLM (docker-compose : mock amont + ferrogate + litellm + k6/oha) — mesurer latence ajoutée p50/p99, RAM, req/s
- [ ] Résultats dans `docs/perf-baseline.md` avec la méthodo exacte (versions, hardware, commandes) — reproductible par n'importe qui
- [ ] Cibles à valider : < 1 ms ajoutée p99 hors réseau, < 25 Mo RAM sous charge, throughput ≥ 95 % du direct

### 7.2 Packaging
- [ ] Binaire statique `x86_64-unknown-linux-musl` + `aarch64` ; vérifier la taille (< 25 Mo strippé)
- [ ] Dockerfile multi-stage → distroless/static, multi-arch (buildx), image < 30 Mo
- [ ] `docker run -v ./config.toml:/config.toml -e OPENAI_API_KEY ghcr.io/.../ferrogate` fonctionne tel quel
- [ ] Release GitHub Actions : binaires + image sur tag `v*`

### 7.3 Hot reload
- [ ] SIGHUP ou watch du fichier config → nouvelle config validée puis swap atomique (ArcSwap du registry) — connexions en cours non affectées
- [ ] Config invalide au reload → log erreur, ancienne config conservée, métrique `config_reload_failures_total`

### 7.4 Sécurité & qualité
- [ ] `cargo audit` + `cargo deny` (licences + advisories) dans la CI
- [ ] Fuzzing léger du parser SSE et de la traduction Anthropic (cargo-fuzz, corpus des fixtures) — 10 min en CI hebdo
- [ ] `SECURITY.md`, en-têtes de sécurité HTTP par défaut

### 7.5 Documentation (déléguer à docs-writer)
- [ ] README avec quickstart 5 minutes, tableau providers×capacités, benchmarks
- [ ] Guides par provider, docs/errors.md complet, CHANGELOG v0.1.0

## Critères d'acceptation
1. `docs/perf-baseline.md` publié avec les 3 cibles atteintes ou l'écart documenté honnêtement.
2. Image Docker : `docker run` du README fonctionne sur amd64 et arm64.
3. Reload avec config cassée → service intact, métrique incrémentée.
4. cargo audit/deny verts.
5. Un nouvel utilisateur peut passer de zéro à une requête chat + embed + rerank réussie en suivant uniquement le README.
