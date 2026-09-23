-- ADR 012 - config source abstraction: the DB config source's version
-- history. Each row is one applied config snapshot; `MAX(id)` is always the
-- current config. Writes go through a compare-and-swap on the previous
-- newest hash (see `KeyStore::insert_config_version`) so two concurrent
-- writers never silently clobber one another, and only the newest 50 rows
-- are retained after each write.
CREATE TABLE config_versions (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    toml       TEXT NOT NULL,
    hash       TEXT NOT NULL,
    applied_at TEXT NOT NULL,  -- RFC 3339 UTC
    actor      TEXT            -- who/what applied it; NULL when unattributed
);
