-- Issue #64: record the provider that actually served each request, so
-- GET /admin/usage can filter and group by provider. Pre-existing rows get
-- the empty string (provider unknown); the writer always sets it (to the
-- provider that served the request, which under a fallback may differ from
-- the primary of the requested model).
ALTER TABLE usage_log ADD COLUMN provider TEXT NOT NULL DEFAULT '';
