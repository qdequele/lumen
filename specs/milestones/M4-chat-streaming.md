# M4 — Chat + streaming SSE

## Objectif
`/v1/chat/completions` complet, streaming zero-copy, traduction Anthropic. Le milestone le plus technique.

## Tâches

### 4.1 Non-streaming
- [ ] `POST /v1/chat/completions` (stream=false) : validation → router → provider → réponse OpenAI
- [ ] Support : messages (system/user/assistant/tool), temperature, max_tokens, stop, tools/tool_choice, response_format json

### 4.2 Streaming SSE
- [ ] `stream=true` : réponse `text/event-stream`, chunks `data: {...}`, terminaison `data: [DONE]`
- [ ] **Passthrough zero-copy** : quand le provider amont parle déjà le format OpenAI (OpenAI, Mistral, Ollama, vLLM), forwarder les frames SSE en `Bytes` SANS désérialiser chaque chunk. Parse minimal uniquement pour extraire l'usage du dernier chunk (comptage des tokens — ADR 003).
- [ ] Comptage des tokens en streaming (ADR 003) : usage du dernier chunk si présent (`estimated=false`) ; sinon compter les tokens out à partir des deltas accumulés / fallback estimation (`estimated=true`) — sans jamais bufferiser le contenu complet ni bloquer.
- [ ] `stream_options: {include_usage: true}` supporté
- [ ] Heartbeat SSE (`: ping`) toutes les 15 s si l'amont est silencieux (keep-alive proxies)
- [ ] Déconnexion client → drop du stream → abort reqwest immédiat (LA leçon LiteLLM #22805)

### 4.3 Provider Anthropic (traduction complète)
- [ ] Requête : messages OpenAI → format Anthropic (system extrait vers `system`, alternance user/assistant normalisée, tools OpenAI → tools Anthropic, max_tokens obligatoire avec défaut configurable)
- [ ] Réponse : content blocks → message OpenAI ; stop_reason → finish_reason ; usage mappé
- [ ] Streaming : événements Anthropic (message_start, content_block_delta, message_delta...) → chunks OpenAI. Ici traduction chunk par chunk obligatoire (pas de passthrough) — état de traduction minimal, zéro buffering du contenu complet
- [ ] tool_use en streaming : input_json_delta → tool_calls delta OpenAI

### 4.4 Providers Mistral + Google
- [ ] Mistral : quasi-passthrough OpenAI
- [ ] Google Gemini : API generateContent + streamGenerateContent, traduction contents/parts, usageMetadata

## Critères d'acceptation
1. Streaming passthrough : test wiremock envoyant 100 chunks → le client reçoit 100 chunks identiques byte-à-byte + [DONE] ; assertion qu'aucune désérialisation complète n'a lieu (compteur dans le code de test ou benchmark d'allocations).
2. Cancellation streaming : client coupe après 3 chunks → la connexion amont wiremock est fermée en < 100 ms (temps simulé).
3. Traduction Anthropic round-trip : fixture de requête OpenAI avec tools → JSON Anthropic exact attendu (snapshot test avec insta) ; idem réponse.
4. Streaming Anthropic : fixture d'événements SSE Anthropic (y compris tool_use) → séquence de chunks OpenAI attendue (snapshot).
5. Amont ferme le stream sans [DONE] → le client reçoit un chunk d'erreur SSE `data: {"error": {"code": "FG-3010"...}}` puis fermeture propre, pas de hang.
6. First-token timeout dépassé → 504 FG-3011 (non-streaming) ou erreur SSE (streaming).
7. Comptage tokens (ADR 003) : streaming avec `include_usage` → tokens in/out du dernier chunk, `estimated=false` ; streaming SANS usage amont → tokens out comptés localement, `estimated=true`, réponse et log non nuls. Non-streaming → usage amont surface tel quel.

## Pièges
- L'état de traduction streaming Anthropic doit être borné : ne JAMAIS accumuler le texte complet en mémoire.
- Les frames SSE amont peuvent être fragmentées à travers les paquets TCP : le parser doit gérer les frames incomplètes (utiliser eventsource-stream ou parser incrémental testé).
