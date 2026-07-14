-- M9: per-request media accounting (a billing dimension alongside tokens).
-- `media_count` is the number of media items (images, …) in the request and
-- `media_bytes` their total DECODED size. Both default to 0 for pre-M9 rows and
-- for text-only requests; the writer always sets them.
ALTER TABLE usage_log ADD COLUMN media_count INTEGER NOT NULL DEFAULT 0;
ALTER TABLE usage_log ADD COLUMN media_bytes INTEGER NOT NULL DEFAULT 0;
