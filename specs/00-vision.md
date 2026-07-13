# Vision — pourquoi LUMEN existe

## Le problème
Les gateways LLM existantes ont des défauts structurels documentés :

**LiteLLM** (Python) : overhead 1.7-4x de throughput mesuré par des utilisateurs en production ; DB dans le chemin de requête (1M de logs → API ralentie) ; 4 Gi RAM/worker recommandés + recyclage de workers pour contenir les fuites ; pas de propagation de cancellation (le GPU continue de générer après déconnexion client) ; readiness probes qui échouent sous charge → cascades de restarts k8s.

**OpenRouter** (SaaS) : pas self-hostable ; pannes de leur propre infra avec des 401 trompeurs ; 5,5 % de frais ; échantillonnage de prompts par défaut ; IDs de modèles qui changent et cassent les intégrations ; pas de hard budget en request path (les agents vident les crédits).

**Tous** : le reranking et les embeddings sont des seconds citoyens, alors que toute stack RAG en a besoin.

## La réponse
Une gateway en Rust : binaire unique, chat + embeddings + rerank égaux, < 1 ms d'overhead, zéro télémétrie, budgets durs atomiques, cancellation de bout en bout, DB hors du chemin critique.

## Utilisateur cible
Le dev/l'équipe qui self-host, mixe APIs cloud (OpenAI, Anthropic, Cohere...) et modèles locaux (Ollama, vLLM, TEI), construit du RAG ou des agents, et veut de la prod fiable sans opérer Postgres+Redis+tuning Gunicorn.

## Principes de décision (quand la spec ne dit rien)
1. En cas de doute : la solution la plus simple qui préserve les 4 piliers (perf, souveraineté, robustesse, multi-capacités).
2. Une feature qui ajoute de la latence au chemin de requête doit être opt-in.
3. La compatibilité OpenAI prime sur l'élégance interne — les clients existants doivent marcher sans modification.
4. Toute donnée utilisateur (prompts, documents) est radioactive : ne jamais la stocker, la logger ou la transmettre ailleurs que vers le provider choisi.
