-- Issue #64: record the provider that served each request, so
-- GET /admin/usage can filter and group by provider. Pre-existing rows get
-- the empty string (provider unknown); the writer always sets it: the
-- provider that actually served the request (under a fallback this may
-- differ from the primary of the requested model), or, for rows recording
-- an admission refusal (402/429, nothing served), the requested model's
-- primary provider.
ALTER TABLE usage_log ADD COLUMN provider TEXT NOT NULL DEFAULT '';
