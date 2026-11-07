-- Sub-groups (ADR 020): a group may name a top-level parent group. A key in a
-- sub-group is admitted against the sub-group AND the parent, so the parent
-- (on the hosted gateway, a team's lease) always binds. NULL = top-level.
-- Set at creation, never changed; one level deep (checked by the store).
ALTER TABLE budget_groups ADD COLUMN parent_id TEXT;
CREATE INDEX idx_budget_groups_parent ON budget_groups (parent_id);
