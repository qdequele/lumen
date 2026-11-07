-- Platform provider key pools (ADR 019): the gateway's own (Meilisearch's)
-- keys per provider, rotated like team keys. Sealed with the master key
-- (AES-256-GCM) like `provider_keys`; `last4` is the only readable part.
CREATE TABLE platform_provider_keys (id TEXT PRIMARY KEY, provider TEXT NOT NULL, label TEXT NOT NULL, last4 TEXT NOT NULL, enabled INTEGER NOT NULL DEFAULT 1, ciphertext BLOB NOT NULL, created_at INTEGER NOT NULL);
CREATE INDEX platform_provider_keys_provider ON platform_provider_keys(provider);
