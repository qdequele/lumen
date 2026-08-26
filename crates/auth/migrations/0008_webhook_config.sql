-- ADR 011 amendment - the webhook configuration as an admin resource.
--
-- A single-row table (guarded by the CHECK on `id`) holding the settings
-- written through PUT /admin/webhooks, plus the HMAC signing secret sealed
-- with AES-256-GCM under the master key when it was provided through
-- PUT /admin/webhooks/signing-key.
--
-- Precedence over the `[webhooks]` config-file block:
--   * a row with enabled = 1 wins outright;
--   * a row with enabled = 0 means off, so a DELETE is not undone by the
--     next config reload;
--   * no row falls back to the file block.
--
-- `events` and `thresholds` are stored as JSON arrays: they are read once at
-- boot and on each apply, never queried element-wise, so a normalized child
-- table would buy nothing and cost a join.

CREATE TABLE webhook_config (
    id              INTEGER PRIMARY KEY CHECK (id = 1),  -- exactly one row
    enabled         INTEGER NOT NULL,                    -- 0 = off, whatever config says
    url             TEXT NOT NULL,
    signing_key_env TEXT,                                -- env var NAME, never a secret
    events          TEXT NOT NULL,                       -- JSON array of dotted event names
    thresholds      TEXT NOT NULL,                       -- JSON array of percentages
    channel_capacity INTEGER NOT NULL,
    timeout_ms      INTEGER NOT NULL,
    max_attempts    INTEGER NOT NULL,
    retry_base_ms   INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL                     -- unix seconds
);

-- The signing secret lives in its own row-less table so rotating it never
-- rewrites (or requires) the settings row, and so the settings row can be
-- dumped in diagnostics without carrying ciphertext.
CREATE TABLE webhook_secret (
    id         INTEGER PRIMARY KEY CHECK (id = 1),
    ciphertext BLOB NOT NULL,                            -- AES-256-GCM: 12-byte nonce || ct
    created_at INTEGER NOT NULL
);
