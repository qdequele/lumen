# M2 — Embeddings : premier chemin de requête complet

## Objectif
`POST /v1/embeddings` fonctionne de bout en bout avec OpenAI et Ollama, batching automatique, cancellation propagée. C'est le milestone qui établit TOUS les patterns (provider, router, tests) — le plus important du projet.

## Tâches

### 2.1 Registry & router
- [ ] `crates/providers/src/registry.rs` : construit les instances de providers depuis la config, expose `get(capability, model_id) -> Option<Arc<dyn ...>>`
- [ ] `crates/router` : résout le modèle demandé → provider, renvoie FG-2001 (modèle inconnu) ou FG-2002 (modèle sans cette capacité) sinon
- [ ] Registry derrière `ArcSwap` (préparation du hot reload M7)

### 2.2 Provider OpenAI (embeddings)
- [ ] `providers/src/openai/` : client reqwest partagé (pool), `embed()` avec traduction minimale (passthrough quasi direct)
- [ ] Gestion `encoding_format` (float | base64), `dimensions`
- [ ] Mapping erreurs : 401→Upstream fatal, 429→RateLimited(retry_after), 5xx→Upstream retryable
- [ ] `max_batch_size()` = 2048 inputs

### 2.3 Provider Ollama (embeddings)
- [ ] `providers/src/ollama/` : API `/api/embed`, traduction schéma Ollama ↔ interne
- [ ] Pas de clé API requise (base_url local) — le code doit accepter les providers sans auth

### 2.4 Batching
- [ ] Si `inputs.len() > provider.max_batch_size()` : découper, exécuter les sous-batches en parallèle (concurrence bornée, défaut 4), réassembler DANS L'ORDRE, sommer les usages
- [ ] Échec d'un sous-batch = échec de la requête entière avec erreur du sous-batch fautif (pas de résultat partiel en v1)

### 2.5 Handler HTTP
- [ ] `POST /v1/embeddings` : validation → router → provider → réponse format OpenAI
- [ ] `CancellationToken` créé par requête, annulé quand la connexion client se ferme (axum : détection via le body/extension), passé jusqu'au `reqwest` (via `select!`)

## Critères d'acceptation
1. Test wiremock : requête 5000 inputs, provider avec max_batch 2048 → exactement 3 appels amont, réponse avec 5000 embeddings dans l'ordre d'origine, usage sommé.
2. Test cancellation : le client drop la connexion pendant l'appel amont → wiremock enregistre la requête amont comme interrompue / le token est annulé avant la fin (assert sur compteur + délai simulé avec start_paused).
3. Test : modèle inconnu → 404 FG-2001 ; modèle chat-only demandé en embedding → 400 FG-2002.
4. Test : amont répond 429 avec Retry-After → réponse 429 au client avec le header propagé et code FG-3001.
5. Test : amont répond du JSON malformé → 502 FG-3002 (jamais 500, jamais de panic).
6. Ollama et OpenAI passent la MÊME suite de tests génériques (macro ou fonction générique de suite de conformité) — ce harnais servira à tous les providers suivants.

## Pattern à établir (réutilisé partout ensuite)
Suite de conformité générique : `fn conformance_suite<P: EmbeddingProvider>(provider: P, mock: MockServer)` exécutée pour chaque provider. Tout nouveau provider DOIT la passer.
