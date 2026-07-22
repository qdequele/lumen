-- ADR 009 - shared parent budgets (budget groups).
--
-- A budget group is a named pool any number of virtual keys can belong to;
-- admission checks the key's own budget AND the group's. Like the rest of
-- the schema there is no SQLite foreign key: referential integrity is
-- enforced at the application layer (store-level validation), keeping the
-- migration additive and the write path simple.

CREATE TABLE budget_groups (
    id           TEXT PRIMARY KEY,
    name         TEXT NOT NULL,
    budget_max   REAL,                       -- NULL = unlimited (USD)
    budget_spent REAL NOT NULL DEFAULT 0,    -- flushed periodically from memory
    created_at   INTEGER NOT NULL,
    deleted_at   INTEGER                     -- soft-delete tombstone, NULL = active
);

ALTER TABLE virtual_keys ADD COLUMN group_id TEXT;

-- Usage attribution: every row (successes and admission refusals alike)
-- is stamped with the key's group at accounting begin, so per-pool
-- reporting includes refused traffic.
ALTER TABLE usage_log ADD COLUMN group_id TEXT;
CREATE INDEX idx_usage_log_group_id ON usage_log (group_id);
