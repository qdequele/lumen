# Kit de bootstrap Ferrogate — mode d'emploi

Ce dossier contient tout ce qu'il faut pour que Claude Code itère en autonomie sur le projet.

## Contenu
```
CLAUDE.md                    # Contexte permanent : architecture, règles, boucle de travail
ROADMAP.md                   # Pilotage : milestones ordonnés avec cases à cocher
.claude/agents/              # 5 subagents spécialisés
├── provider-integrator.md   # Ajout de providers (le travail le plus répétitif)
├── test-writer.md           # TDD : tests avant implémentation
├── code-reviewer.md         # Review read-only avant chaque commit
├── perf-auditor.md          # Audit perf du chemin de requête
└── docs-writer.md           # Docs utilisateur
specs/
├── 00-vision.md             # Le pourquoi (ancre les décisions ambiguës)
└── milestones/M1..M7        # Specs détaillées avec critères d'acceptation testables
```

## Démarrage
```bash
mkdir ferrogate && cd ferrogate
git init
# copier le contenu de ce kit à la racine
claude
```

Premier prompt suggéré :
> Lis CLAUDE.md, ROADMAP.md et specs/00-vision.md. Commence le milestone M1 en suivant la boucle de travail : test-writer d'abord, implémentation ensuite, code-reviewer avant chaque commit.

## Pour les sessions suivantes
> Reprends là où on en est : lis ROADMAP.md, identifie le milestone courant et continue.

## Conseils pour l'autonomie
- **Une session ≈ un milestone** (ou une moitié pour M4/M5). Ne pas enchaîner deux milestones dans la même session : le contexte se dégrade.
- Utilise `/compact` si la session devient longue, ou redémarre — la ROADMAP et les cases cochées portent l'état, pas la conversation.
- Après M2, l'ajout de providers devient parallélisable : "Utilise le subagent provider-integrator pour Cohere, Jina et TEI en parallèle."
- Lance le subagent code-reviewer explicitement en fin de milestone si Claude ne le fait pas spontanément.
- Vérifie toi-même les critères d'acceptation d'un milestone avant de laisser Claude passer au suivant (5 min de lecture des tests suffisent).
- Mode autonome : `claude --dangerously-skip-permissions` accélère mais fais-le dans un conteneur/VM dédié ; sinon configure les permissions dans `.claude/settings.json` (allow: cargo, git commit ; deny: git push).

## Fichiers que Claude Code créera lui-même
`CHANGELOG.md`, `docs/adr/`, `docs/errors.md`, `docs/backlog.md`, `docs/perf-baseline.md`, `config.example.toml`, et tout le code.

---

## Resilience (M6)

Ferrogate keeps a request alive across a flaky upstream without ever
destabilising the gateway itself. Four mechanisms compose, all configured in
the `[resilience]` section of `config.toml` (see `config.example.toml` for the
fully commented version) plus a per-model `fallbacks` list. Everything below is
on by default with sensible values; nothing here touches the database on the
request path.

### Retries

Retryable upstream failures (5xx, connect/read timeout, 429) are retried with
exponential backoff and equal jitter. An upstream `Retry-After` is honoured as a
floor. A client `4xx` is never retried — a different provider would reject it
too. While streaming, a retry only happens if no chunk has yet reached the
client.

### Fallback chains

A model can name an ordered list of fallback models. They are tried, in order,
once the primary has exhausted its retries or its circuit is open. Every
fallback must exist and serve every capability of the model it backs — this is
validated at boot, so a typo fails startup rather than a live request. The model
that actually served is reported in the `x-ferrogate-model-used` response header
and recorded in `usage_log.model_used`.

```toml
[[providers.models]]
id = "gpt-4o"
capabilities = ["chat"]
fallbacks = ["claude-3-5-sonnet"]   # tried if OpenAI is down / circuit open
```

### Circuit breaker

Per `(provider, model)`: after `circuit_failure_threshold` consecutive
provider-fault failures (default 5) the circuit opens; the model is then skipped
straight to its next fallback. After `circuit_cooldown_ms` (default 30 s) a
single half-open probe decides whether to close again. An open circuit with no
fallback left returns `503 FG-3020` with a `Retry-After`.

### Timeouts

Three distinct timeouts, each surfacing a distinct `504` for debugging:

| Phase       | Config                                    | Default | Code      |
|-------------|-------------------------------------------|---------|-----------|
| connect     | `resilience.connect_timeout_ms` (global)  | 5 s     | `FG-3012` |
| first token | `server.first_token_timeout_ms`           | 30 s    | `FG-3011` |
| total       | `resilience.total_timeout_ms`             | 600 s   | `FG-3013` |

`connect` is client-wide (one pooled HTTP client). The first-token and total
timeouts can be overridden per provider with `first_token_timeout_ms` /
`total_timeout_ms` on a `[[providers]]` block; the total deadline bounds all
retries and fallbacks together.

### Background health checks

Off by default. When enabled, a periodic task probes every provider that has a
configured `base_url` (self-hosted TEI/Ollama, or an explicit override) and
publishes the result at `GET /health/providers`. Providers on a built-in vendor
URL report `unknown` (never probed). The gateway's own `GET /health` stays fully
independent of provider health and does no I/O.

```toml
[resilience]
retry_max_attempts = 3
circuit_failure_threshold = 5
circuit_cooldown_ms = 30000
total_timeout_ms = 600000
health_check_enabled = true
health_check_interval_ms = 30000
```

### Metrics

Two Prometheus gauges expose resilience state on `GET /metrics`:

- `ferrogate_circuit_state{provider,model}` — `0` closed, `1` open, `2` half-open.
- `ferrogate_provider_up{provider}` — `1` up, `0` down (background health checks;
  a provider that is never probed simply reports no sample).

See `docs/errors.md` for the full `FG-3xxx` code reference and
`docs/adr/005-resilience-execution.md` for the design rationale.
