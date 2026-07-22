-- M5 §5.1 - virtual keys, usage log, encrypted provider keys.
--
-- Deliberately NO prompt/response columns anywhere: LUMEN never persists
-- request or response content (sovereignty pillar).

CREATE TABLE virtual_keys (
    id           TEXT PRIMARY KEY,
    key_hash     TEXT NOT NULL UNIQUE,
    name         TEXT NOT NULL,
    budget_max   REAL,                       -- NULL = unlimited (USD)
    budget_spent REAL NOT NULL DEFAULT 0,    -- flushed periodically from memory
    rpm_limit    INTEGER,                    -- NULL = unlimited
    tpm_limit    INTEGER,                    -- NULL = unlimited
    expires_at   INTEGER,                    -- unix seconds, NULL = never
    disabled     INTEGER NOT NULL DEFAULT 0,
    created_at   INTEGER NOT NULL
);

CREATE TABLE usage_log (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    key_id       TEXT,                       -- NULL when auth is disabled
    model        TEXT NOT NULL,
    capability   TEXT NOT NULL,              -- chat | embed | rerank
    tokens_in    INTEGER NOT NULL,
    tokens_out   INTEGER NOT NULL,
    search_units INTEGER,                    -- rerank only, NULL elsewhere
    estimated    INTEGER NOT NULL DEFAULT 0, -- ADR 003: measured vs estimated
    cost         REAL NOT NULL DEFAULT 0,    -- USD, derived from config prices
    latency_ms   INTEGER NOT NULL,
    status       INTEGER NOT NULL,           -- HTTP status returned to client
    metadata     TEXT,                       -- ADR 002: flat JSON object or NULL
    ts           INTEGER NOT NULL            -- unix seconds
);
CREATE INDEX idx_usage_log_ts ON usage_log (ts);
CREATE INDEX idx_usage_log_key_id ON usage_log (key_id);

CREATE TABLE provider_keys (
    name       TEXT PRIMARY KEY,
    ciphertext BLOB NOT NULL,                -- AES-256-GCM: 12-byte nonce || ct
    created_at INTEGER NOT NULL
);
