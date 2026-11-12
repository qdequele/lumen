-- ADR 023 - cursor pagination of the admin listings. A page of
-- `GET /admin/keys` or `GET /admin/groups` is a keyset read ordered by
-- `(created_at, id)`, both immutable; these indexes let a page seek to its
-- cursor and stop after `limit` rows instead of sorting the table.
-- Additive: no row changes.
CREATE INDEX IF NOT EXISTS idx_virtual_keys_created ON virtual_keys (created_at, id);
CREATE INDEX IF NOT EXISTS idx_budget_groups_created ON budget_groups (created_at, id);
