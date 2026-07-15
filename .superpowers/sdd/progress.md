# Fix-all-issues execution ledger

Strategy: one PR per issue, branch from origin/main, TDD, full validation gate.
User decisions: include #7 and #24 (amend ADR 005); one PR per issue.

## Wave 1 - small independent fixes (PARALLEL, worktree-isolated)
- [ ] #6  Anthropic real created timestamp (scope narrowed to timestamp only)
- [ ] #13 LM-2004 pre-flight scans full fallback chain
- [ ] #21 --check-config subcommand
- [ ] #22 Cohere embed input_type override
- [ ] #4  Gemini tool calling (functionDeclarations mapping or LM-2002)

## Wave 2 - shared core-type changes (SEQUENTIAL, stacked if files overlap)
- [ ] #11 client-cancel error variant (error.rs)
- [ ] #10 token-based rerank usage Jina/Voyage (rerank.rs)
- [ ] #9  per-image vision token heuristic (tokens.rs, chat.rs)
- [ ] #25 embed/rerank input format gaps (embed.rs, rerank.rs)
- [ ] #12 provider-native file/GCS image sources (chat.rs)

## Wave 3 - new providers (PARALLEL; expect mechanical conflicts in kind.rs/registry.rs)
- [ ] #17 Cohere chat
- [ ] #18 Cloudflare Workers AI rerank
- [ ] #14 Azure OpenAI
- [ ] #16 Google Vertex AI
- [ ] #15 AWS Bedrock
- [ ] #19 additional rerankers (Mixedbread, Pinecone, NVIDIA NIM, Together)

## Wave 4 - infra/ops
- [ ] #23 richer per-kind health probes
- [ ] #20 hot reload extension + DB key rotation
- [ ] #26 rate-limit & usage-log accounting refinements
- [ ] #8  accurate per-model tokenizer (opt-in)
- [ ] #7  first-frame-peek streaming retry (AMEND ADR 005)
- [ ] #24 per-provider connect timeout (AMEND ADR 005)
- [ ] #27 test & benchmark debt (LAST - locks tests over everything)

## Completed
- #6  PR #34 - review clean. COMPLETE.
- #13 PR #35 - review clean. Minor: CHANGELOG wording says "covers whole chain" but fix is reclassification (PR title fixed by controller). COMPLETE.
- #21 PR #38 - review clean. COMPLETE.
- #4  PR #37 - review clean. Minors for final review: empty functionResponse.name fallback (google/mod.rs:360); JSON Schema passthrough to Gemini unvalidated (accepted v1 limitation, matches Anthropic). COMPLETE.

- #22 PR #36 - fix 933b399 re-reviewed clean (leak gone, failing-first regression tests verified). COMPLETE.
- #10 PR #41 - review clean. Note for final review: pre-existing silent-zero edge (empty query + empty docs -> estimated 0). COMPLETE.
- #9  PR #40 - review clean. Minor: detail=="low" matched case-sensitively (consistent w/ repo convention). COMPLETE.

- #12 PR #43 - fix 5391287 re-reviewed APPROVED. Minor for final review: no server test for ordinary-remote-URL LM-2004 (pre-existing gap). COMPLETE.

- #25 PR #44 - fix 8e3b2a5 re-reviewed APPROVED. COMPLETE.
- #18 PR #45 - review APPROVED (no findings). COMPLETE.
- #16 PR #47 - review APPROVED. Minors: inert 401 literal in ready(); project_id not config-overridable. COMPLETE.
- #17 PR #50 - review APPROVED. Caveat recorded: v2 wire schema unverified against live Cohere API; live smoke test prudent before prod. COMPLETE.

## In flight
- #11 PR #39 - re-review stalled mid-compile; pinged reviewer to finish.
- #14 PR #46 (Azure) - review approved w/ fixes requested: URL percent-encoding of deployment/api-version (Important), multi-query-param test (Minor). Fix dispatched.
- #15 PR #49 (Bedrock) - CRITICAL: SigV4 canonical path single-encoded, must be double-encoded for non-S3 services; breaks ALL colon model ids (every Claude). + Important: region mis-derived for custom endpoints; credential staleness; KAT gap. Fix dispatched.
- #19 PR #48 (rerankers) - CRITICAL: Mixedbread endpoint wrong (/rerank vs real POST /v1/reranking), also baked into docs/config. Fix dispatched.
- #23 PR #51 (health probes) - implemented, awaiting review.
- #20 PR #54 (hot reload) - implemented, awaiting review. Claims ADR 006 (COLLISION).
- #26 PR #53 (accounting) - implemented, awaiting review. Claims ADR 006 (COLLISION).
- #8  PR #52 (tokenizer) - implemented, awaiting review. tiktoken-rs 0.12 dep.
- #14 PR #46 - re-review APPROVED (dfc2d19). COMPLETE.
- #15 PR #49 - re-review APPROVED "Mergeable" (481ab1f, AWS KATs independently recomputed). COMPLETE.
- #19 PR #48 - re-review APPROVED (160e06b). COMPLETE.
- #23 PR #51 - re-review APPROVED (aca9f86). COMPLETE.
- #24 PR #55 - review APPROVED. Minors: blackhole-IP test portability; stale backstop on reload. COMPLETE.
- #8  PR #52 - re-review APPROVED (8dee0c4, deferred refinement). Minors: detached refinement tasks not drained on shutdown (follow-up); mark_completed misuse hardening. COMPLETE.
- #7  PR #56 - review APPROVED. Minor: first-frame budget now single-window (was effectively 2x); note in ADR. COMPLETE.
- #27 PR #57 - review APPROVED. Important recorded as follow-up: signal test leaks child process on assert failure (add Drop-guard kill). Minors: backlog wording, bench cosmetics, local path in committed k6 logs. COMPLETE.
- #11 PR #39 - re-review APPROVED (fails-before independently reproduced). COMPLETE.

## ALL 23 ISSUES COMPLETE - 23 PRs, all review-approved.

## MERGE TRAIN (user directive: rebase onto main every time, then merge; gate on CI + local gate)
- Chunk A MERGED: #34 #38 #37 #40 #35 #39 #43 (pre-merged by user) + #44 (cdb2fa3) #36 (df08733) #41 (1452bf3). ADR 006 = client-cancellation (from #39, merged first, keeps number).
- Chunk B IN PROGRESS (agent af9624f9eba843319): #45 #46 #47 #50 #48 #49.
- Chunk C PENDING: #51 #53(ADR->007) #54(ADR->008) #55 #52 #56 #57 LAST.
Merge-time coordination needed:
1. ADR renumbering: #39/#53/#54 each add "ADR 006" -> renumber to 006/007/008 in merge order (+ fix in-text refs).
2. ADR 005: #55 and #56 both append amendment sections -> keep both.
3. Mechanical conflicts: CHANGELOG (all PRs), core/error.rs enum additions (#35,#39,#43,#44,#56), server/chat.rs (#35,#39,#43,#52,#56), kind.rs/registry.rs (provider PRs #45-#50,#55), core/rerank.rs (#41,#44), resilience tests (#35,#56,#57).
4. Suggested merge order: #34,#38,#37,#40 -> #35 -> #39 -> #43 -> #44 -> #36,#41 -> #45,#46,#47,#50,#48,#49 -> #51,#53,#54,#55 -> #52 -> #56 -> #57 LAST (contains flaky-test fix + signal tests over final tree).
5. Follow-ups filed in backlog by agents: api_version config field (Azure), authenticated vendor health probes, per-model input_type default, non-streaming 5xx usage_log gap doc fix, signal-test Drop guard.
- #20 PR #54 - review APPROVED. Minors: ADR 006 collision; Notify wake path untested e2e; flush first-tick delay. COMPLETE.
- #26 PR #53 - review APPROVED. Minors: ADR 006 collision; ADR overstates non-streaming 5xx usage_log coverage (one-line doc fix at renumber pass). COMPLETE.
- #24 PR #55 - implemented (dedicated client only for overriding providers; ADR 005 amended), awaiting review.
- #23 PR #51 - CRITICAL from review: vLLM probe {base}/health but documented vllm base_url includes /v1 -> /v1/health 404s -> healthy server marked DOWN (regression vs bare reachability). Fix dispatched.
- #8 PR #52 - CRITICAL from review: accurate BPE await is PRE-response in handlers, violating ADR 003 "never a blocking BPE pass ... never slow a request" (should be async usage-writer side). + rerank counts unconditionally; no settle-path integration test. Fix dispatched.
- #46/#48/#49 fixes pushed (dfc2d19/160e06b/481ab1f), re-reviews dispatched.
- #7 and #27 dispatched (parallel). NOTE: #7 amends ADR 005 which PR #55 also amends -> expected doc conflict.
- Corrupted ~/.cargo cc-1.2.67 extraction removed by controller (was breaking fresh builds; cargo re-extracts).
- ADR NUMBERING COLLISION: PR #39, #53, #54 all add an "ADR 006". Renumber before/at merge (renumber pass at the end).
- New flake seen once: mistral_passes_embed_conformance_suite (delay-based, same family as 429-storm flake).

## Known environment issue
- crates/server/tests/resilience.rs `health_stays_fast_under_upstream_429_storm` is flaky on clean main (connect-storm FD race). Confirmed by 5 independent runs. Spawn-task chip filed (task_fe696eda).
