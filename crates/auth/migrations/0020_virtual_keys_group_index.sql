-- An account-scoped key listing (`GET /admin/keys` with X-Lumen-Account-Ref)
-- selects keys by `group_id IN (the account's groups)`. Without an index on
-- `group_id` that scanned every key; the member-count checks on a group
-- delete read the same column. Additive: no row changes.
CREATE INDEX IF NOT EXISTS idx_virtual_keys_group_id ON virtual_keys (group_id);
