# M6 — Résilience

## Objectif
Retries, fallbacks, circuit breaker, timeouts — sans jamais compromettre la stabilité de la gateway elle-même sous charge (leçon LiteLLM #15526 : cascade de restarts k8s sous 429 amont).

## Tâches

### 6.1 Retries
- [x] Retry sur `ProviderError` retryable uniquement (5xx, timeout connect, 429) — jamais sur 4xx client
- [x] Backoff exponentiel + jitter (défaut : base 200 ms, max 5 s, 3 tentatives), honore `Retry-After` s'il est plus long
- [x] Streaming : retry SEULEMENT si aucun chunk n'a encore été émis au client
- [x] Budget de retry global par requête (le temps total reste borné par le timeout total)

### 6.2 Fallback
- [x] Config : `fallbacks = ["model-a", "model-b"]` par modèle — même capacité exigée, validée au boot
- [x] Fallback déclenché après épuisement des retries du provider courant
- [x] Header de réponse `x-lumen-model-used` + champ dans usage_log
- [x] Même règle streaming : pas de fallback après le premier chunk émis

### 6.3 Circuit breaker
- [x] Par (provider, modèle) : Closed → Open après N échecs consécutifs (défaut 5) → Half-Open après cooldown (défaut 30 s) → 1 requête sonde
- [x] Circuit ouvert → skip immédiat vers le fallback ; si aucun fallback : 503 LM-3020 avec Retry-After
- [x] État exposé dans /metrics (`circuit_state{provider,model}`)

### 6.4 Timeouts
- [x] Trois timeouts configurables par provider avec défauts globaux : `connect` (5 s), `first_token` (30 s), `total` (600 s)
- [x] Chaque timeout → erreur distincte (LM-3011/3012/3013) pour le debugging

Note : `connect` est un réglage client global (un seul client HTTP mutualisé) —
pas d'override par provider ; `first_token` et `total` sont overridables par
provider. LM-3011 = first-token, LM-3012 = connect, LM-3013 = total. Voir
`docs/adr/005-resilience-execution.md`.

### 6.5 Health checks de fond
- [x] Tâche périodique optionnelle (défaut off) qui sonde les providers — résultats en mémoire + métrique, JAMAIS consultés dans le request path de façon bloquante
- [x] /health de la gateway reste indépendant de la santé des providers ; ajouter `/health/providers` séparé pour l'observabilité

## Critères d'acceptation
1. Test : amont 500 puis 500 puis 200 → succès, 3 appels wiremock, délais de backoff respectés (temps simulé start_paused).
2. Test : provider A épuise ses retries → bascule sur B, réponse OK, header x-lumen-model-used = B.
3. Test : 5 échecs → circuit Open → la 6e requête ne touche PAS l'amont (compteur wiremock) et fallback immédiatement ; après cooldown, 1 sonde passe.
4. Test streaming : échec après 2 chunks émis → PAS de retry ni fallback, erreur SSE propre.
5. Test de charge : 500 requêtes concurrentes vers un amont qui répond 429 → /health répond < 10 ms pendant toute la durée, RAM stable (pas de file d'attente non bornée).
6. Test : Retry-After: 3 → attente d'au moins 3 s (temps simulé).
