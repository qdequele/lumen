# M5 — Auth, clés virtuelles & budgets durs

## Objectif
Clés virtuelles avec budgets DURS enforced dans le chemin de requête — le gap que ni LiteLLM ni OpenRouter ne comblent bien (workloads agentiques qui vident les crédits sans stop). Et la DB reste HORS du chemin critique.

## Tâches

### 5.1 Stockage
- [x] sqlx + SQLite, migrations embarquées (`sqlx::migrate!`)
- [x] Tables : `virtual_keys(id, key_hash, name, budget_max, budget_spent, rpm_limit, tpm_limit, expires_at, disabled)`, `usage_log(id, key_id, model, capability, tokens_in, tokens_out, search_units, cost, latency_ms, status, ts)` — PAS de colonne prompt/response
- [x] Clés virtuelles : `fg-` + 32 bytes random ; stockage argon2/blake3 du hash, jamais le clair
- [x] Clés providers optionnellement en DB, chiffrées AES-256-GCM (master key via env `FERROGATE_MASTER_KEY`) ; le mode par défaut reste les env vars

### 5.2 Enforcement dans le request path — SANS toucher la DB
- [x] État des clés (budget restant, compteurs RPM/TPM) chargé en mémoire au boot dans un `DashMap`/`ArcSwap`
- [x] Vérification budget/quota = lecture mémoire + CAS atomique. Requête refusée AVANT l'appel amont : 402 FG-4001 (budget épuisé), 429 FG-4002 (RPM), 429 FG-4003 (TPM)
- [x] Débit du budget : estimation pré-appel (max_tokens) réservée atomiquement, ajustée post-appel avec l'usage réel — pas de course possible entre requêtes concurrentes
- [x] Persistance : flush périodique (défaut 10 s) des compteurs mémoire → DB. Crash = perte de max 10 s de comptage, jamais de dépassement de budget non détecté à la requête suivante

### 5.3 Logging d'usage asynchrone
- [x] Channel mpsc BORNÉ (défaut 10 000) → tâche writer qui batch les INSERT (défaut : toutes les 2 s ou 500 entrées)
- [x] Channel plein → drop du log + compteur Prometheus `usage_log_dropped_total` incrémenté. Le request path ne bloque JAMAIS sur le logging (leçon LiteLLM #12067)
- [x] Rétention configurable : purge des usage_log > N jours (tâche de fond, défaut 30 j)

### 5.4 Comptage des tokens (promesse centrale — voir ADR 003)
- [x] **Un compte de tokens pour CHAQUE requête, toute capacité**, jamais `0` par défaut : chat (in + out), embeddings (in), rerank (search_units si dispo + tokens query+documents)
- [x] Source prioritaire : usage rapporté par l'amont (`estimated = false`) ; sinon fallback estimation (`estimated = true`)
- [x] Fallback : heuristique légère (byte/char) par défaut, tokenizer précis optionnel par modèle (config) exécuté via `spawn_blocking` — JAMAIS de tokenizer lourd sur le chemin de requête (pilier 1) *(heuristique implémentée ; le tokenizer précis opt-in part en backlog — dépendance lourde, voir `docs/backlog.md` § M5 — l'invariant « jamais de tokenizer lourd inline » tient par construction)*
- [x] TEI (aucun usage amont) → tokens estimés, jamais zéro silencieux
- [x] Compteurs Prometheus à cardinalité fixe : `ferrogate_tokens_total{capability,model,provider,direction,estimated}`, `ferrogate_rerank_search_units_total{model,provider}`, `tokens_estimated_total`
- [x] Le comptage ne bloque ni ne fait échouer JAMAIS une requête ; l'estimation précise se fait hors du hot path (dans le writer async)

### 5.4b Comptage des coûts (consommateur des tokens ci-dessus)
- [x] Table de prix par modèle dans la config (`cost_per_1m_input`, `cost_per_1m_output`, `cost_per_1k_searches`)
- [x] Coût dérivé des tokens comptés en 5.4 ; embeddings : tokens in seulement ; rerank : search units
- [x] Usage extrait du dernier chunk en streaming ; si absent, estimation et flag `estimated: true` dans le log et la réponse

### 5.5 API d'admin minimale
- [x] `POST/GET/PATCH /admin/keys` protégé par la master key — créer/lister/désactiver des clés, ajuster les budgets
- [x] La réponse de création est la SEULE fois où la clé claire est visible

### 5.6 Métadonnées de requête (style Cloudflare AI Gateway) — voir ADR 002
- [x] Header `x-ferrogate-metadata` (+ alias `cf-aig-metadata`) : objet JSON PLAT `clé → (string|number|bool)`, parsé une fois au bord dans les extensions de requête (zéro alloc si absent)
- [x] Bornes : ≤ 16 clés, clé ≤ 64 o, valeur ≤ 256 o, header ≤ 4 Kio
- [x] Sink logs : la métadonnée complète est attachée aux champs du log structuré ET stockée dans une colonne `metadata` de `usage_log` (filtrage à la Cloudflare)
- [x] Sink Prometheus : SEULES les clés de l'allowlist config (`telemetry.metadata_labels`, défaut vide) deviennent des labels ; les autres restent logs-only (cardinalité bornée par l'opérateur, jamais par le client)
- [x] Robustesse : métadonnée absente/malformée/hors-bornes → droppée avec `warn!` + compteur `metadata_rejected_total`, la requête N'ÉCHOUE JAMAIS
- [x] Sécurité : métadonnée opaque, jamais inspectée ; documenter qu'elle est loggée et ne doit pas contenir de secret ni de contenu de prompt

## Critères d'acceptation

> Statut : critères 1–11 couverts par des tests automatisés (`crates/server/tests/auth.rs`
> au niveau HTTP + `crates/auth/tests/*` au niveau unitaire). Notes :
> * Critère 3 : le chemin de requête ne touche JAMAIS la DB par construction ;
>   testé avec un writer mort (stand-in d'une DB verrouillée) — 70 requêtes
>   passent à pleine vitesse, chaque entrée droppée est comptée.
> * Critère 5 : moitié DB testée par dump complet (`debug_dump`) ; côté logs,
>   le plaintext n'est jamais passé à `tracing` et `PlaintextKey`/`MasterKey`
>   ont un `Debug` caviardé (testé unitairement).
> * Critère 12 : sans objet en l'état — seul l'heuristique O(octets) inline
>   existe ; le tokenizer précis (le seul risque de latence) est en backlog et
>   devra embarquer ce test de latence quand il arrivera.
1. Test de course : 50 requêtes concurrentes sur une clé avec budget pour 10 → exactement les requêtes couvertes par le budget passent, zéro dépassement (assert sur le compteur atomique final).
2. Test : budget épuisé → 402 AVANT tout appel amont (wiremock : zéro requête reçue).
3. Test : DB verrouillée/lente (simulée) → les requêtes API continuent de passer, seul le flush est retardé ; latence p99 du chemin de requête inchangée.
4. Test : channel de logs saturé → requêtes non bloquées, compteur dropped incrémenté.
5. Test : la clé virtuelle claire n'apparaît ni en DB ni dans les logs (grep sur logs capturés + dump DB).
6. Test : redémarrage → budgets rechargés depuis la DB, une clé épuisée reste épuisée.
7. Test : `x-ferrogate-metadata` valide → apparaît dans le log d'usage ; seules les clés de l'allowlist deviennent des labels Prometheus ; une clé hors allowlist n'ajoute AUCune série temporelle.
8. Test : métadonnée malformée ou > bornes → requête réussit quand même, `metadata_rejected_total` incrémenté, rien dans les labels.
9. Test : embeddings via TEI (amont sans usage) → le log ET `ferrogate_tokens_total` rapportent un compte > 0 avec `estimated="true"` ; jamais zéro.
10. Test : embeddings via OpenAI (amont avec usage) → compte = valeur amont, `estimated="false"`.
11. Test : chaque capacité (chat/embed/rerank) incrémente `ferrogate_tokens_total` avec le bon `capability`/`direction` ; rerank incrémente aussi `ferrogate_rerank_search_units_total`.
12. Test : latence p99 du chemin de requête inchangée quand l'estimation par tokenizer est activée (l'estimation reste hors hot path).
