-- Team provider key pools (BYOK): a budget group (one hosted Lab account)
-- brings its own API keys for a provider. The key is sealed with the master
-- key (AES-256-GCM) like `provider_keys`; `last4` is the only readable part.
CREATE TABLE group_provider_keys (id TEXT PRIMARY KEY, group_id TEXT NOT NULL REFERENCES budget_groups(id), provider TEXT NOT NULL, label TEXT NOT NULL, last4 TEXT NOT NULL, ciphertext BLOB NOT NULL, created_at INTEGER NOT NULL);
CREATE INDEX group_provider_keys_group ON group_provider_keys(group_id, provider);
-- Per (group, provider) pool settings: `fallback` = fall back to the
-- platform's keys when every team key is rate-limited or failing.
CREATE TABLE group_provider_settings (group_id TEXT NOT NULL REFERENCES budget_groups(id), provider TEXT NOT NULL, fallback INTEGER NOT NULL DEFAULT 0, PRIMARY KEY (group_id, provider));
