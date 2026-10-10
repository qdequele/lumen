-- Platform contract v2 section 3.5: an event the Lab answered but left out
-- of `accepted` for 24 hours is permanently rejected. A dropped row keeps
-- its body for the operator and is purged with delivered rows after 7 days.
ALTER TABLE usage_outbox ADD COLUMN dropped_ms INTEGER;
-- Dropped rows are rare: a partial index keeps the hourly purge on indexes
-- (a multi-index OR with idx_usage_outbox_due) instead of a table scan, at
-- no write cost for the pending and delivered rows it leaves out.
CREATE INDEX idx_usage_outbox_dropped ON usage_outbox (dropped_ms) WHERE dropped_ms IS NOT NULL;
