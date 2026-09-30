-- ADR 015 - Meilisearch Lab integration: opaque control-plane refs and the
-- billing watermark.

-- The Lab account a budget group (the account's lease) belongs to.
ALTER TABLE budget_groups ADD COLUMN account_ref TEXT;
CREATE INDEX idx_budget_groups_account_ref ON budget_groups (account_ref);

-- The control plane's own id for a key, reported as `api_key_id`.
ALTER TABLE virtual_keys ADD COLUMN external_ref TEXT;
CREATE INDEX idx_virtual_keys_external_ref ON virtual_keys (external_ref);

-- Billing watermark in micro-USD: the part of budget_spent already turned
-- into usage events. Back-filled to the current spend so enabling
-- [usage_events] on an upgraded gateway never bills past spend.
ALTER TABLE virtual_keys ADD COLUMN billed_micro INTEGER NOT NULL DEFAULT 0;
UPDATE virtual_keys SET billed_micro = CAST(ROUND(budget_spent * 1000000) AS INTEGER);
