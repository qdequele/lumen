-- Issue #99: per-request token breakdown (cached / reasoning / cache-write).
-- Nullable INTEGER columns so an absent upstream breakdown stays NULL rather
-- than a fabricated 0 (ADR 003: never zero, never invented). `cached_tokens`
-- is prompt tokens served from cache (cache read), `reasoning_tokens` the
-- completion-side reasoning split, and `cache_write_tokens` prompt tokens
-- written to cache (Anthropic cache-creation, which has no OpenAI equivalent).
ALTER TABLE usage_log ADD COLUMN cached_tokens INTEGER;
ALTER TABLE usage_log ADD COLUMN reasoning_tokens INTEGER;
ALTER TABLE usage_log ADD COLUMN cache_write_tokens INTEGER;
