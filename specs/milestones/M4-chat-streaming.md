# M4 — Chat + streaming SSE

## Objectif
`/v1/chat/completions` complet, streaming zero-copy, traduction Anthropic. Le milestone le plus technique.

## Tâches

### 4.1 Non-streaming
- [x] `POST /v1/chat/completions` (stream=false) : validation → router → provider → réponse OpenAI
- [x] Support : messages (system/user/assistant/tool), temperature, max_tokens, stop, tools/tool_choice, response_format json (passthrough via `extra`)

### 4.2 Streaming SSE
- [x] `stream=true` : réponse `text/event-stream`, chunks `data: {...}`, terminaison `data: [DONE]`
- [x] **Passthrough zero-copy** : quand le provider amont parle déjà le format OpenAI (OpenAI, Mistral, Ollama, vLLM), forwarder les frames SSE en `Bytes` SANS désérialiser chaque chunk (ADR 004 ; `chat_stream_bytes` + `http::open_stream`). Prouvé byte-à-byte sur 100 chunks.
- [x] Comptage des tokens en streaming (ADR 003), moitié « usage amont » : passthrough via `stream_options.include_usage` demandé automatiquement ; traduction Anthropic/Gemini → usage complet dans le chunk final. *(La moitié « estimation locale `estimated=true` » part en M5 avec les compteurs Prometheus et `usage_log`.)*
- [x] `stream_options: {include_usage: true}` supporté (demandé automatiquement à l'amont, sans écraser un choix client)
- [x] Heartbeat SSE (`: ping`) toutes les 15 s (configurable `sse_heartbeat_ms`) si l'amont est silencieux (keep-alive proxies)
- [x] Déconnexion client → drop du stream → abort reqwest immédiat (LA leçon LiteLLM #22805) : drop-guard movné dans le body

### 4.3 Provider Anthropic (traduction complète)
- [x] Requête : messages OpenAI → format Anthropic (system extrait vers `system`, tool_results consécutifs fusionnés en un message user, tools OpenAI → tools Anthropic + tool_choice, max_tokens obligatoire avec défaut)
- [x] Réponse : content blocks → message OpenAI (tool_use → tool_calls, arguments ré-encodés en string JSON) ; stop_reason → finish_reason ; usage mappé
- [x] Streaming : événements Anthropic (message_start, content_block_delta, message_delta...) → chunks OpenAI, traduction chunk par chunk via parser SSE incrémental — état borné, zéro buffering du contenu complet
- [x] tool_use en streaming : content_block_start → ouverture tool_call, input_json_delta → tool_calls delta OpenAI (index allocation en ordre d'apparition)

### 4.4 Providers Mistral + Google
- [x] Mistral : quasi-passthrough OpenAI (chat + embeddings)
- [x] Google Gemini : `generateContent` (non-streaming) et `streamGenerateContent?alt=sse` (streaming) traduits — contents/parts, systemInstruction, usageMetadata, finishReason.

## Critères d'acceptation
1. Streaming passthrough : test wiremock envoyant 100 chunks → le client reçoit 100 chunks identiques byte-à-byte + [DONE] ; assertion qu'aucune désérialisation complète n'a lieu (compteur dans le code de test ou benchmark d'allocations).
2. Cancellation streaming : client coupe après 3 chunks → la connexion amont wiremock est fermée en < 100 ms (temps simulé).
3. Traduction Anthropic round-trip : fixture de requête OpenAI avec tools → JSON Anthropic exact attendu (snapshot test avec insta) ; idem réponse.
4. Streaming Anthropic : fixture d'événements SSE Anthropic (y compris tool_use) → séquence de chunks OpenAI attendue (snapshot).
5. Amont ferme le stream sans [DONE] → le client reçoit un chunk d'erreur SSE `data: {"error": {"code": "FG-3010"...}}` puis fermeture propre, pas de hang.
6. First-token timeout dépassé → 504 FG-3011 (non-streaming) ou erreur SSE (streaming).
7. Comptage tokens (ADR 003) : streaming avec `include_usage` → tokens in/out du dernier chunk, `estimated=false` ; streaming SANS usage amont → tokens out comptés localement, `estimated=true`, réponse et log non nuls. Non-streaming → usage amont surface tel quel. *(Satisfait pour la moitié « usage amont » ; l'estimation locale `estimated=true` est vérifiée en M5 avec l'infra télémétrie.)*

## Pièges
- L'état de traduction streaming Anthropic doit être borné : ne JAMAIS accumuler le texte complet en mémoire.
- Les frames SSE amont peuvent être fragmentées à travers les paquets TCP : le parser doit gérer les frames incomplètes (utiliser eventsource-stream ou parser incrémental testé).
