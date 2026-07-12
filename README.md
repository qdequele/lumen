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
